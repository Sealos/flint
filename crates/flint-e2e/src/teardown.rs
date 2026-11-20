//! [`Teardown`]: what every app e2e test checks when the app goes away, whatever the test did
//! (docs/TESTING.md, "Teardown checks"):
//!
//! - **UP-91:** no fake API server received a request from the app before the test selected a
//!   cluster on it, and a server the test never selected received none at all. Counted from the
//!   app's launch, per server, on the fake servers the app's kubeconfig names (a kind cluster has
//!   no request log, so it is not checked).
//! - **UP-90:** every kubeconfig file the app was given is byte-identical at the end, and one that
//!   did not exist still does not.
//! - The app exited on Quit (not killed) and removed its automation socket.
//!
//! The exceptions are explicit: a cluster selected at startup with `FLINT_CLUSTER`
//! ([`crate::app::AppOptions::cluster_at_startup`]), one selected another way or contacted by the
//! test itself ([`crate::app::App::mark_selected`]), and kubeconfig edits the test makes
//! ([`crate::app::App::edit_kubeconfig`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use crate::fake_api::{RequestRecord, arrivals_at, connection_mark_at};

/// How many of a server's unexpected requests a violation lists.
const LISTED: usize = 5;

/// One fake server the app could reach.
#[derive(Debug)]
struct Server {
    /// The server's connection mark when the app launched (or when the kubeconfig first named
    /// it): only requests on later connections are the app's. A client that came before (the
    /// app this one relaunches, the test itself) may still have requests on its way on its own
    /// connections. Requests count on arrival, so one a fault delays counts from when it was
    /// sent.
    since: u64,
    selected: bool,
}

/// The kubeconfig files the app reads now (its settings can add some during a test).
pub type Files = Box<dyn Fn() -> Vec<PathBuf> + Send>;

/// The state the teardown checks need, kept from the app's launch.
pub struct Teardown {
    files: Files,
    /// The kubeconfig files the app has read, and their bytes (`None`: missing) when first seen
    /// or last edited by the test.
    kubeconfigs: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// Context name → API server URLs it has had during the test.
    contexts: BTreeMap<String, BTreeSet<String>>,
    /// Fake servers by URL.
    servers: BTreeMap<String, Server>,
    violations: Vec<String>,
}

impl Teardown {
    /// Snapshots the kubeconfig `files` and the request counts of the fake servers they name,
    /// before the app starts.
    pub fn at_launch(files: Files) -> Teardown {
        let mut teardown = Teardown {
            files,
            kubeconfigs: Vec::new(),
            contexts: BTreeMap::new(),
            servers: BTreeMap::new(),
            violations: Vec::new(),
        };
        teardown.snapshot_kubeconfigs();
        teardown.refresh_contexts();
        teardown
    }

    /// The test changed the kubeconfig files on purpose (UP-06): their new bytes are the ones
    /// checked at the end.
    pub fn kubeconfig_edited(&mut self) {
        self.snapshot_kubeconfigs();
        self.refresh_contexts();
    }

    /// The app is about to read `files` (settings the test is about to write list them): their
    /// bytes, and the request counts of the fake servers they name, are taken now, before the
    /// app can read or contact them.
    pub fn expect_files(&mut self, files: Vec<PathBuf>) {
        for path in files {
            if !self.kubeconfigs.iter().any(|(p, _)| *p == path) {
                let bytes = std::fs::read(&path).ok();
                self.kubeconfigs.push((path, bytes));
            }
        }
        self.refresh_contexts();
    }

    /// The test selects `context` now: checks that the app has not contacted its servers yet,
    /// and allows requests to them from here on.
    pub fn selecting(&mut self, context: &str) {
        self.refresh_contexts();
        let urls = self.contexts.get(context).cloned().unwrap_or_default();
        for url in urls {
            let Some(server) = self.servers.get_mut(&url) else {
                continue;
            };
            if server.selected {
                continue;
            }
            server.selected = true;
            let requests = since(&url, server.since);
            if !requests.is_empty() {
                self.violations.push(unexpected(
                    &format!("before the test selected {context:?}"),
                    &url,
                    &requests,
                ));
            }
        }
    }

    /// Checks the app's end, given how it exited (or why it could not be stopped) and where its
    /// socket was, and returns every violation of the test, or nothing.
    pub fn finish(&mut self, exit: Result<ExitStatus, String>, socket: &Path) -> Vec<String> {
        self.refresh_contexts();
        let mut violations = std::mem::take(&mut self.violations);
        for (url, server) in &self.servers {
            if server.selected {
                continue;
            }
            let requests = since(url, server.since);
            if !requests.is_empty() {
                violations.push(unexpected(
                    &format!(
                        "though the test never selected {}",
                        self.contexts_of(url).join(" or ")
                    ),
                    url,
                    &requests,
                ));
            }
        }
        for (path, before) in &self.kubeconfigs {
            let after = std::fs::read(path).ok();
            if after != *before {
                let change = match (before, &after) {
                    (None, Some(_)) => "was created",
                    (Some(_), None) => "was removed",
                    _ => "changed",
                };
                violations.push(format!(
                    "UP-90: the kubeconfig {} {change} during the test, and not through \
                     App::edit_kubeconfig",
                    path.display()
                ));
            }
        }
        match exit {
            Ok(status) if status.success() => {}
            Ok(status) => violations.push(format!("the app did not exit cleanly: {status}")),
            Err(error) => violations.push(format!("the app did not quit: {error}")),
        }
        if socket.symlink_metadata().is_ok() {
            violations.push(format!(
                "the app left its automation socket behind: {}",
                socket.display()
            ));
        }
        violations
    }

    fn snapshot_kubeconfigs(&mut self) {
        self.add_new_files();
        for (path, bytes) in &mut self.kubeconfigs {
            *bytes = std::fs::read(&*path).ok();
        }
    }

    /// Starts checking files the app reads that were not seen before, as they are now.
    fn add_new_files(&mut self) {
        for path in (self.files)() {
            if !self.kubeconfigs.iter().any(|(p, _)| *p == path) {
                let bytes = std::fs::read(&path).ok();
                self.kubeconfigs.push((path, bytes));
            }
        }
    }

    /// Reads the contexts the kubeconfig files name now, and starts counting for fake servers
    /// seen for the first time. A server that a context the test already selected now names
    /// counts as selected.
    fn refresh_contexts(&mut self) {
        self.add_new_files();
        let paths: Vec<PathBuf> = self.kubeconfigs.iter().map(|(p, _)| p.clone()).collect();
        let loaded = flint_core::kubeconfig::load(&paths);
        for context in loaded.contexts {
            let Some(url) = context.server else { continue };
            let url = url.trim_end_matches('/').to_owned();
            if !self.servers.contains_key(&url)
                && let Some(mark) = connection_mark_at(&url)
            {
                let selected = self.contexts.get(&context.name).is_some_and(|urls| {
                    urls.iter()
                        .any(|u| self.servers.get(u).is_some_and(|s| s.selected))
                });
                self.servers.insert(
                    url.clone(),
                    Server {
                        since: mark,
                        selected,
                    },
                );
            }
            self.contexts.entry(context.name).or_default().insert(url);
        }
    }

    fn contexts_of(&self, url: &str) -> Vec<String> {
        self.contexts
            .iter()
            .filter(|(_, urls)| urls.contains(url))
            .map(|(name, _)| format!("{name:?}"))
            .collect()
    }
}

/// The requests that have arrived at the fake server at `url` on connections after `mark`.
fn since(url: &str, mark: u64) -> Vec<RequestRecord> {
    arrivals_at(url)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.connection > mark)
        .collect()
}

/// A UP-91 violation: `requests` reached `url` `when`.
fn unexpected(when: &str, url: &str, requests: &[RequestRecord]) -> String {
    let mut text = format!(
        "UP-91: {} request(s) reached the fake server at {url} {when}:",
        requests.len()
    );
    for request in requests.iter().take(LISTED) {
        let _ = write!(
            text,
            "\n    {} {}?{}",
            request.method, request.path, request.query
        );
    }
    if requests.len() > LISTED {
        let _ = write!(text, "\n    … and {} more", requests.len() - LISTED);
    }
    text
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::*;
    use crate::fake_api::{FakeApiServer, Options, write_kubeconfigs};

    fn files(paths: Vec<PathBuf>) -> Teardown {
        Teardown::at_launch(Box::new(move || paths.clone()))
    }

    fn ok() -> Result<ExitStatus, String> {
        Ok(ExitStatus::from_raw(0))
    }

    /// Two fake servers in one kubeconfig, and a client the "app" uses.
    struct Two {
        a: FakeApiServer,
        b: FakeApiServer,
        dir: tempfile::TempDir,
        runtime: tokio::runtime::Runtime,
    }

    impl Two {
        fn start() -> Two {
            let a = FakeApiServer::start_blocking(Options::default()).unwrap();
            let b = FakeApiServer::start_blocking(Options::default()).unwrap();
            let dir = tempfile::tempdir().unwrap();
            write_kubeconfigs(
                &dir.path().join("kubeconfig"),
                &[(&a, "a", None), (&b, "b", None)],
            )
            .unwrap();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            Two { a, b, dir, runtime }
        }

        fn kubeconfig(&self) -> PathBuf {
            self.dir.path().join("kubeconfig")
        }

        /// One request to `server`, as the app would send.
        fn contact(&self, server: &FakeApiServer) {
            self.runtime.block_on(async {
                let client = server.client().await.unwrap();
                client.apiserver_version().await.unwrap();
            });
        }

        fn teardown(&self) -> Teardown {
            files(vec![self.kubeconfig()])
        }
    }

    #[test]
    fn requests_after_selection_pass() {
        let two = Two::start();
        // Before launch: the test's own requests are not the app's.
        two.contact(&two.a);
        let mut teardown = two.teardown();
        teardown.selecting("a");
        two.contact(&two.a);
        let gone = two.dir.path().join("no-socket");
        assert_eq!(teardown.finish(ok(), &gone), Vec::<String>::new());
    }

    // A client that was there before the launch (the app a test relaunches) may still have a
    // request on its way on its own connection: it is not the new app's.
    #[test]
    fn a_late_request_on_an_older_connection_is_not_the_apps() {
        let two = Two::start();
        let client = two.runtime.block_on(two.a.client()).unwrap();
        two.runtime.block_on(client.apiserver_version()).unwrap();
        let mut teardown = two.teardown();
        // On the pooled connection opened before the launch.
        two.runtime.block_on(client.apiserver_version()).unwrap();
        let arrivals = two.a.arrivals();
        assert_eq!(arrivals.len(), 2);
        assert_eq!(
            arrivals[0].connection, arrivals[1].connection,
            "the second request reuses the first one's connection"
        );
        // A new client's request after the launch counts.
        two.contact(&two.b);
        let violations = teardown.finish(ok(), &two.dir.path().join("no-socket"));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("never selected \"b\""),
            "{violations:?}"
        );
    }

    #[test]
    fn a_request_before_selection_fails() {
        let two = Two::start();
        let mut teardown = two.teardown();
        two.contact(&two.a);
        teardown.selecting("a");
        let violations = teardown.finish(ok(), &two.dir.path().join("no-socket"));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].starts_with("UP-91: 1 request(s) reached the fake server at")
                && violations[0].contains("before the test selected \"a\"")
                && violations[0].contains("GET /version"),
            "{violations:?}"
        );
    }

    #[test]
    fn a_request_to_an_unselected_cluster_fails() {
        let two = Two::start();
        let mut teardown = two.teardown();
        teardown.selecting("a");
        two.contact(&two.a);
        two.contact(&two.b);
        let violations = teardown.finish(ok(), &two.dir.path().join("no-socket"));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains(&two.b.url()) && violations[0].contains("never selected \"b\""),
            "{violations:?}"
        );
    }

    #[test]
    fn a_changed_or_created_kubeconfig_fails_unless_the_test_edited_it() {
        let two = Two::start();
        let missing = two.dir.path().join("missing");
        let mut teardown = files(vec![two.kubeconfig(), missing.clone()]);
        let socket = two.dir.path().join("no-socket");

        std::fs::write(two.kubeconfig(), "edited by the test").unwrap();
        teardown.kubeconfig_edited();
        assert_eq!(teardown.finish(ok(), &socket), Vec::<String>::new());

        std::fs::write(two.kubeconfig(), "rewritten by the app").unwrap();
        std::fs::write(&missing, "created by the app").unwrap();
        let violations = teardown.finish(ok(), &socket);
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(
            violations[0].contains("kubeconfig")
                && violations[0]
                    .ends_with("changed during the test, and not through App::edit_kubeconfig")
        );
        assert!(violations[1].contains("was created"), "{violations:?}");
    }

    #[test]
    fn a_kill_or_a_left_socket_fails() {
        let two = Two::start();
        let mut teardown = two.teardown();
        let socket = two.dir.path().join("automation.sock");
        std::fs::write(&socket, "").unwrap();
        // SIGKILL.
        let violations = teardown.finish(Ok(ExitStatus::from_raw(9)), &socket);
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(violations[0].starts_with("the app did not exit cleanly"));
        assert!(violations[1].starts_with("the app left its automation socket"));
        let violations = teardown.finish(Err("a dialog".into()), &two.dir.path().join("none"));
        assert_eq!(violations, ["the app did not quit: a dialog"]);
    }

    #[test]
    fn servers_added_to_the_kubeconfig_count_from_then() {
        let two = Two::start();
        let path = two.kubeconfig();
        write_kubeconfigs(&path, &[(&two.a, "a", None)]).unwrap();
        let mut teardown = two.teardown();
        // Before b was in the kubeconfig: the app could not have sent this.
        two.contact(&two.b);
        write_kubeconfigs(&path, &[(&two.a, "a", None), (&two.b, "b", None)]).unwrap();
        teardown.kubeconfig_edited();
        teardown.selecting("b");
        two.contact(&two.b);
        let violations = teardown.finish(ok(), &two.dir.path().join("no-socket"));
        assert_eq!(violations, Vec::<String>::new());
    }

    #[test]
    fn a_selected_context_pointed_at_another_server_stays_selected() {
        let two = Two::start();
        let path = two.kubeconfig();
        write_kubeconfigs(&path, &[(&two.a, "x", None)]).unwrap();
        let mut teardown = two.teardown();
        teardown.selecting("x");
        write_kubeconfigs(&path, &[(&two.b, "x", None)]).unwrap();
        teardown.kubeconfig_edited();
        two.contact(&two.b);
        let violations = teardown.finish(ok(), &two.dir.path().join("no-socket"));
        assert_eq!(violations, Vec::<String>::new());
    }

    #[test]
    fn files_the_app_starts_reading_are_checked_from_then() {
        let two = Two::start();
        let later = two.dir.path().join("later");
        std::fs::write(&later, "before").unwrap();
        let listed = std::sync::Arc::new(std::sync::Mutex::new(vec![two.kubeconfig()]));
        let current = listed.clone();
        let mut teardown = Teardown::at_launch(Box::new(move || current.lock().unwrap().clone()));
        listed.lock().unwrap().push(later.clone());
        teardown.selecting("a");
        std::fs::write(&later, "after").unwrap();
        let violations = teardown.finish(ok(), &two.dir.path().join("no-socket"));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("later changed"), "{violations:?}");
    }

    #[test]
    fn files_expected_before_settings_name_them_count_from_then() {
        let two = Two::start();
        let path = two.kubeconfig();
        let only_a = two.dir.path().join("only-a");
        write_kubeconfigs(&only_a, &[(&two.a, "a", None)]).unwrap();
        let mut teardown = files(vec![only_a]);
        // Settings are about to add the file with b: its bytes and b's count are taken now.
        teardown.expect_files(vec![path.clone()]);
        two.contact(&two.b);
        std::fs::write(&path, "rewritten").unwrap();
        let violations = teardown.finish(ok(), &two.dir.path().join("no-socket"));
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(
            violations[0].contains("never selected \"b\""),
            "{violations:?}"
        );
        assert!(violations[1].starts_with("UP-90"), "{violations:?}");
    }

    #[test]
    fn a_request_still_delayed_at_selection_counts_as_before_it() {
        let two = Two::start();
        let mut teardown = two.teardown();
        two.a
            .inject(crate::fake_api::Fault::delay(std::time::Duration::from_secs(2)).times(1));
        // The request arrives before the selection but is answered after it.
        let started = std::time::Instant::now();
        let a = &two.a;
        std::thread::scope(|scope| {
            scope.spawn(|| two.contact(a));
            while a.arrivals().is_empty() {
                assert!(started.elapsed() < std::time::Duration::from_secs(10));
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(a.requests().is_empty(), "not answered yet");
            teardown.selecting("a");
        });
        let violations = teardown.finish(ok(), &two.dir.path().join("no-socket"));
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("before the test selected \"a\"")
                && violations[0].contains("GET /version"),
            "{violations:?}"
        );
    }

    #[test]
    fn kind_and_unknown_contexts_are_not_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kubeconfig");
        std::fs::write(
            &path,
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster: {server: \"https://127.0.0.1:1\"}\nusers:\n- name: u\n  user: {}\ncontexts:\n- name: kind\n  context: {cluster: c, user: u}\n",
        )
        .unwrap();
        let mut teardown = files(vec![path]);
        teardown.selecting("kind");
        teardown.selecting("not-in-the-kubeconfig");
        assert!(teardown.servers.is_empty());
        assert_eq!(
            teardown.finish(ok(), &dir.path().join("no-socket")),
            Vec::<String>::new()
        );
    }
}
