//! Cluster sets (UP-82 to UP-84): choosing clusters, saving a set, and the combined list, driven
//! through the set actions against fake API servers.

use anyhow::{Context as _, Result, ensure};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::fake_api::{FakeApiServer, Fault, Gvr, Options, write_kubeconfigs};
use http::Method;
use serde_json::{Value, json};

use crate::support::*;

const A: &str = TwoClusters::A;
const B: &str = TwoClusters::B;
const ROWS: &str = "/sets/open/list/rows";

/// Replaces the pods of `fake` with `pods`, as `(name, status)` in the namespace `demo`.
fn seed_pods(fake: &FakeApiServer, pods: &[(&str, &str)]) {
    let gvr = Gvr::core("pods");
    for row in fake.rows(&gvr) {
        fake.remove_row(&gvr, row.namespace(), row.name());
    }
    for (name, status) in pods {
        let row = fake.new_row(&gvr, Some("demo"), name, &[("Status", status)]);
        fake.set_row(&gvr, row);
    }
}

/// Chooses `clusters` in the sidebar and opens them together. Opening contacts each of them.
fn open_together(app: &App, clusters: &[&str]) -> Result<()> {
    pick(app, clusters)?;
    app.dispatch("cluster_sets::OpenPickedClusters", Value::Null)?;
    app.wait_for("/sets/open", Exists(true))?;
    Ok(())
}

fn pick(app: &App, clusters: &[&str]) -> Result<()> {
    app.dispatch("cluster_sets::ToggleClusterPicking", Value::Null)?;
    app.wait_for("/sets/picking", Equals(true.into()))?;
    for cluster in clusters {
        app.mark_selected(cluster);
        app.dispatch(
            "cluster_sets::TogglePickedCluster",
            json!({ "cluster": cluster }),
        )?;
    }
    app.wait_for("/sets/picked", Equals(json!(clusters)))?;
    Ok(())
}

/// The combined rows, in order, as `(cluster, name)`.
fn shown(state: &Value) -> Vec<(String, String)> {
    open_rows(state)
        .iter()
        .map(|r| (cell(r, "Cluster"), text(&r["name"])))
        .collect()
}

fn open_rows(state: &Value) -> Vec<Value> {
    state["sets"]["open"]["list"]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn cell(row: &Value, column: &str) -> String {
    text(&row["cells_by_column"][column])
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(c, n)| ((*c).to_owned(), (*n).to_owned()))
        .collect()
}

fn member<'a>(state: &'a Value, cluster: &str) -> &'a Value {
    state["sets"]["open"]["members"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["cluster"] == cluster))
        .unwrap_or(&Value::Null)
}

fn assert_no_writes(fakes: &[&FakeApiServer]) -> Result<()> {
    for fake in fakes {
        ensure!(fake.writes().is_empty(), "{:?}", fake.writes());
    }
    Ok(())
}

/// GETs of exactly `path` that reached `fake`, answered or not.
fn arrived_gets(fake: &FakeApiServer, path: &str) -> usize {
    fake.arrivals()
        .iter()
        .filter(|r| r.method == Method::GET && r.path == path)
        .count()
}

/// GETs of exactly `path` that `fake` answered with a 200.
fn loaded_gets(fake: &FakeApiServer, path: &str) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.method == Method::GET && r.path == path && r.status == Some(200))
        .count()
}

/// Lets what the app sent just before it exited reach the fake servers' logs.
fn drain_after_quit() {
    std::thread::sleep(std::time::Duration::from_millis(250));
}

/// Waits until the navigation badges `kind` with `badge`, opening any collapsed section.
fn wait_for_badge(app: &App, kind: &str, badge: &str) -> Result<()> {
    until(
        &format!("{kind} badged {badge}"),
        || {
            let state = app.state().unwrap_or(Value::Null);
            let sections = state["nav"]["sections"].as_array().cloned();
            let mut found = Value::Null;
            for section in sections.unwrap_or_default() {
                if section["collapsed"] == true {
                    let title = section["title"].clone();
                    let _ = app.dispatch("workspace::ToggleNavSection", json!({"section": title}));
                }
                if let Some(entry) = section["entries"]
                    .as_array()
                    .and_then(|e| e.iter().find(|e| e["kind"] == kind))
                {
                    found = entry["badge"].clone();
                }
            }
            found
        },
        |found| found == badge,
    )?;
    Ok(())
}

// covers UP-82
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_82_combine_clusters_into_one_view() -> Result<()> {
    let two = TwoClusters::start()?;
    seed_pods(
        &two.a,
        &[("api", "Running"), ("crash-a", "CrashLoopBackOff")],
    );
    seed_pods(
        &two.b,
        &[("api", "Running"), ("crash-b", "CrashLoopBackOff")],
    );
    let tags = format!(
        "[[cluster_tags]]\ncontext = \"{A}\"\nserver = \"{}\"\ntag = \"red\"\n",
        two.a.url()
    );
    let app = App::launch(AppOptions::new(two.kubeconfig()).settings(&tags))?;
    connect(&app, A)?;
    open_together(&app, &[A, B])?;

    let state = app.wait_for(ROWS, Test::at_least(4))?;
    let open = &state["sets"]["open"];
    assert_eq!(open["label"], "2 clusters");
    assert_eq!(open["list"]["kind"], "pods");
    assert_eq!(open["list"]["columns"][0], "Cluster");
    assert_eq!(member(&state, A)["tag"], "red");
    assert_eq!(member(&state, B)["tag"], Value::Null);
    assert_eq!(state["sets"]["picking"], false, "opening ends the choice");

    // Sorted by Status, the crashing pods of both clusters come first.
    app.dispatch(
        "combined_list::SortCombined",
        json!({"key": "server:Status", "occurrence": 0}),
    )?;
    let state = app.wait_for(
        "/sets/open/list/sort",
        Equals(json!({"column": "Status", "descending": false})),
    )?;
    let statuses: Vec<String> = open_rows(&state)
        .iter()
        .map(|r| cell(r, "Status"))
        .collect();
    assert_eq!(
        statuses,
        ["CrashLoopBackOff", "CrashLoopBackOff", "Running", "Running"]
    );
    let mut crashing: Vec<_> = shown(&state)[..2].to_vec();
    crashing.sort();
    assert_eq!(crashing, pairs(&[(A, "crash-a"), (B, "crash-b")]));

    // Grouped: one section per cluster over the same rows.
    app.dispatch("combined_list::ToggleGroupByCluster", Value::Null)?;
    let state = app.wait_for("/sets/open/groups", Test::at_least(2))?;
    assert_eq!(
        state["sets"]["open"]["groups"],
        json!([
            {"cluster": A, "count": 2, "collapsed": false},
            {"cluster": B, "count": 2, "collapsed": false},
        ])
    );
    assert_eq!(
        shown(&state),
        pairs(&[(A, "crash-a"), (A, "api"), (B, "crash-b"), (B, "api")])
    );
    app.dispatch("combined_list::ToggleClusterGroup", json!({"cluster": B}))?;
    let state = app.wait_for(ROWS, Test::at_most(2))?;
    assert_eq!(shown(&state), pairs(&[(A, "crash-a"), (A, "api")]));
    assert_eq!(state["sets"]["open"]["groups"][1]["count"], 2);
    app.dispatch("combined_list::ToggleClusterGroup", json!({"cluster": B}))?;
    app.wait_for(ROWS, Test::at_least(4))?;

    // A row of the same name opens from its own cluster.
    const PATH: &str = "/api/v1/namespaces/demo/pods/api";
    let uid = two
        .b
        .row(&Gvr::core("pods"), Some("demo"), "api")
        .context("the pod on b")?
        .uid()
        .to_owned();
    let on_a = arrived_gets(&two.a, PATH);
    let on_b = loaded_gets(&two.b, PATH);
    app.dispatch(
        "combined_list::SelectCombinedRow",
        json!({"cluster": B, "uid": uid}),
    )?;
    let state = app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    assert_eq!(state["detail"]["name"], "api");
    assert_eq!(state["detail"]["namespace"], "demo");
    until(
        "a successful read on b",
        || loaded_gets(&two.b, PATH),
        |n| *n > on_b,
    )?;

    app.quit()?;
    drain_after_quit();
    assert_eq!(
        arrived_gets(&two.a, PATH),
        on_a,
        "the other cluster was not read"
    );
    assert_no_writes(&[&two.a, &two.b])?;
    Ok(())
}

// covers UP-83
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_83_save_and_reopen_a_cluster_set() -> Result<()> {
    let two = TwoClusters::start()?;
    seed_pods(&two.a, &[("on-a", "Running")]);
    seed_pods(&two.b, &[("on-b", "Running")]);
    let replacement = FakeApiServer::start_blocking(Options::default())?;
    seed_pods(&replacement, &[("on-replacement", "Running")]);

    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    connect(&app, A)?;
    pick(&app, &[A, B])?;
    app.dispatch("cluster_sets::SaveClusterSet", json!({"name": "regions"}))?;
    app.wait_for("/sets/saved", Equals(json!(["regions"])))?;

    // Saved with each member pinned to its server.
    let settings: toml::Table =
        std::fs::read_to_string(app.config_dir().join("settings.toml"))?.parse()?;
    let saved = &settings["cluster_sets"][0];
    assert_eq!(saved["name"].as_str(), Some("regions"));
    let members: Vec<_> = saved["members"]
        .as_array()
        .context("the saved members")?
        .iter()
        .map(|m| (m["context"].as_str(), m["server"].as_str()))
        .collect();
    let (url_a, url_b) = (two.a.url(), two.b.url());
    assert_eq!(
        members,
        [
            (Some(A), Some(url_a.as_str())),
            (Some(B), Some(url_b.as_str()))
        ]
    );

    // After a relaunch the same clusters open combined.
    let scratch = app.quit()?;
    let app = App::launch(AppOptions::new(two.kubeconfig()).reuse(scratch))?;
    app.wait_for("/sets/saved", Equals(json!(["regions"])))?;
    connect(&app, A)?;
    app.mark_selected(B);
    app.dispatch("cluster_sets::OpenClusterSet", json!({"name": "regions"}))?;
    let state = app.wait_for(ROWS, Test::at_least(2))?;
    assert_eq!(state["sets"]["open"]["label"], "regions");
    let mut rows = shown(&state);
    rows.sort();
    assert_eq!(rows, pairs(&[(A, "on-a"), (B, "on-b")]));

    // A member re-pointed while open is marked changed and loses its rows; the new server's
    // rows never appear under the set.
    app.edit_kubeconfig([two.kubeconfig()], || {
        write_kubeconfigs(
            &two.kubeconfig(),
            &[(&two.a, A, None), (&replacement, B, None)],
        )
    })?;
    let state = app.wait_for(
        "/sets/open/members",
        Any(json!({"cluster": B, "note": "Changed"})),
    )?;
    assert_eq!(member(&state, A)["note"], Value::Null);
    let state = app.wait_for(ROWS, Test::at_most(1))?;
    assert_eq!(shown(&state), pairs(&[(A, "on-a")]));

    app.quit()?;
    drain_after_quit();
    ensure!(
        replacement.arrivals().is_empty(),
        "the replacement server was contacted: {:?}",
        replacement.arrivals()
    );
    assert_no_writes(&[&two.a, &two.b, &replacement])?;
    Ok(())
}

// covers UP-83
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_83_a_repointed_member_is_left_out_when_the_set_opens() -> Result<()> {
    const C: &str = "kind-fake-c";
    let two = TwoClusters::start()?;
    let c = FakeApiServer::start_blocking(Options::default())?;
    let replacement = FakeApiServer::start_blocking(Options::default())?;
    seed_pods(&two.a, &[("on-a", "Running")]);
    seed_pods(&two.b, &[("on-b", "Running")]);
    seed_pods(&c, &[("on-c", "Running")]);
    seed_pods(&replacement, &[("on-replacement", "Running")]);
    let kubeconfig = two.kubeconfig();
    write_kubeconfigs(
        &kubeconfig,
        &[(&two.a, A, None), (&two.b, B, None), (&c, C, None)],
    )?;

    let app = App::launch(AppOptions::new(&kubeconfig))?;
    connect(&app, A)?;
    pick(&app, &[A, B, C])?;
    app.dispatch("cluster_sets::SaveClusterSet", json!({"name": "regions"}))?;
    app.wait_for("/sets/saved", Equals(json!(["regions"])))?;
    let scratch = app.quit()?;
    drain_after_quit();

    write_kubeconfigs(
        &kubeconfig,
        &[(&two.a, A, None), (&replacement, B, None), (&c, C, None)],
    )?;
    let on_old_b = two.b.arrivals().len();
    let app = App::launch(AppOptions::new(&kubeconfig).reuse(scratch))?;
    app.wait_for("/sets/saved", Equals(json!(["regions"])))?;
    connect(&app, A)?;
    app.mark_selected(C);
    app.dispatch("cluster_sets::OpenClusterSet", json!({"name": "regions"}))?;
    let state = app.wait_for(ROWS, Test::at_least(2))?;

    let open = &state["sets"]["open"];
    assert_eq!(open["left_out"], json!([B]));
    let members: Vec<_> = open["members"]
        .as_array()
        .context("the members")?
        .iter()
        .map(|m| text(&m["cluster"]))
        .collect();
    assert_eq!(members, [A, C]);
    let mut rows = shown(&state);
    rows.sort();
    assert_eq!(rows, pairs(&[(A, "on-a"), (C, "on-c")]));

    app.quit()?;
    drain_after_quit();
    ensure!(
        replacement.arrivals().is_empty(),
        "the replacement server was contacted: {:?}",
        replacement.arrivals()
    );
    assert_eq!(
        two.b.arrivals().len(),
        on_old_b,
        "the old server was left alone"
    );
    assert_no_writes(&[&two.a, &two.b, &c, &replacement])?;
    Ok(())
}

// covers UP-83
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_83_a_set_error_never_quotes_the_settings_file() -> Result<()> {
    const CANARY: &str = "e2e-canary-credential";
    let two = TwoClusters::start()?;
    let malformed = format!(
        "[[cluster_sets]]\nname = \"broken\"\n[[cluster_sets.members]]\ncontext = \"{A}\"\n\
         server = \"https://flint:{CANARY}@bad host\n"
    );
    let app = App::launch(AppOptions::new(two.kubeconfig()).settings(&malformed))?;

    // Loading the saved sets.
    let state = app.wait_for("/sets/error", Exists(true))?;
    let error = text(&state["sets"]["error"]);
    ensure!(error.starts_with("invalid TOML"), "{error}");
    ensure!(!error.contains(CANARY), "{error}");

    // Saving over the same file.
    connect(&app, A)?;
    pick(&app, &[A, B])?;
    app.wait_for("/sets/error", Exists(false))?;
    app.dispatch("cluster_sets::SaveClusterSet", json!({"name": "regions"}))?;
    let state = app.wait_for("/sets/error", Exists(true))?;
    let error = text(&state["sets"]["error"]);
    ensure!(error.starts_with("invalid TOML"), "{error}");
    ensure!(!error.contains(CANARY), "{error}");

    app.quit()?;
    drain_after_quit();
    assert_no_writes(&[&two.a, &two.b])?;
    Ok(())
}

// covers UP-84
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_84_columns_and_versions_that_differ() -> Result<()> {
    let configmaps = Gvr::core("configmaps");
    let a = FakeApiServer::start_blocking(Options {
        version: Some("v1.31.4".to_owned()),
        ..Options::default()
    })?;
    // Answers without Tables, so it returns none of the server's own columns.
    let b = FakeApiServer::start_blocking(Options {
        version: Some("v1.29.8".to_owned()),
        plain_only: vec![configmaps.clone()],
        ..Options::default()
    })?;
    for (fake, name) in [(&a, "only-a"), (&b, "only-b")] {
        for row in fake.rows(&configmaps) {
            fake.remove_row(&configmaps, row.namespace(), row.name());
        }
        fake.set_object(
            &configmaps,
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": name, "namespace": "demo", "uid": name,
                             "resourceVersion": "1"},
                "data": {"key": "value"},
            }),
        );
    }
    let row = a.new_row(&configmaps, Some("demo"), "only-a", &[("Data", "1")]);
    a.set_row(&configmaps, row);
    let scratch = flint_e2e::scratch::Scratch::new()?;
    let kubeconfig = scratch.path().join("kubeconfig");
    write_kubeconfigs(&kubeconfig, &[(&a, A, None), (&b, B, None)])?;

    let app = App::launch(AppOptions::new(&kubeconfig))?;
    connect(&app, A)?;
    open_together(&app, &[A, B])?;
    app.open_view("configmaps")?;
    app.wait_for("/sets/open/list/kind", Equals("configmaps".into()))?;
    let state = app.wait_for(ROWS, Test::at_least(2))?;

    let rows = open_rows(&state);
    let row = |name: &str| rows.iter().find(|r| r["name"] == name);
    let on_a = row("only-a").context("a's row")?;
    let on_b = row("only-b").context("b's row")?;
    assert_eq!(cell(on_a, "Cluster"), A);
    assert_eq!(cell(on_b, "Cluster"), B);
    assert_eq!(cell(on_a, "Data"), "1", "the column a returns");
    assert_eq!(
        on_b["cells_by_column"]["Data"],
        json!(""),
        "blank where b has no such column"
    );

    app.quit()?;
    drain_after_quit();
    assert_no_writes(&[&a, &b])?;
    Ok(())
}

// covers UP-84
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_84_kinds_that_only_some_members_serve() -> Result<()> {
    const C: &str = "kind-fake-c";
    const GADGETS: &str = "gadgets.fixtures.flint.dev";
    const INGRESSES: &str = "ingresses.networking.k8s.io";
    let gadgets = Gvr::new("fixtures.flint.dev", "v1", "gadgets");
    let ingresses = Gvr::new("networking.k8s.io", "v1", "ingresses");
    // The active member serves neither kind; only b serves ingresses.
    let a = FakeApiServer::start_blocking(Options {
        hidden: vec![gadgets.clone(), ingresses.clone()],
        ..Options::default()
    })?;
    let b = FakeApiServer::start_blocking(Options::default())?;
    let c = FakeApiServer::start_blocking(Options {
        hidden: vec![ingresses.clone()],
        ..Options::default()
    })?;
    let on_b = b.rows(&ingresses).len();
    ensure!(on_b > 0, "the fixtures hold ingresses");
    let scratch = flint_e2e::scratch::Scratch::new()?;
    let kubeconfig = scratch.path().join("kubeconfig");
    write_kubeconfigs(&kubeconfig, &[(&a, A, None), (&b, B, None), (&c, C, None)])?;

    let app = App::launch(AppOptions::new(&kubeconfig))?;
    connect(&app, A)?;
    open_together(&app, &[A, B, C])?;
    wait_for_badge(&app, GADGETS, "2/3")?;
    wait_for_badge(&app, INGRESSES, "1/3")?;

    app.open_view(INGRESSES)?;
    app.wait_for("/sets/open/list/kind", Equals(INGRESSES.into()))?;
    let state = app.wait_for(ROWS, Test::at_least(on_b as u32))?;
    ensure!(
        shown(&state).iter().all(|(cluster, _)| cluster == B),
        "{:?}",
        shown(&state)
    );

    app.open_view(GADGETS)?;
    app.wait_for("/sets/open/list/kind", Equals(GADGETS.into()))?;
    let expected = b.rows(&gadgets).len() + c.rows(&gadgets).len();
    let state = app.wait_for(ROWS, Test::at_least(expected as u32))?;
    ensure!(
        shown(&state).iter().all(|(cluster, _)| cluster != A),
        "{:?}",
        shown(&state)
    );

    app.quit()?;
    drain_after_quit();
    assert_no_writes(&[&a, &b, &c])?;
    Ok(())
}

// covers UP-84
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_84_a_crd_on_one_member_names_the_other() -> Result<()> {
    let gadgets = Gvr::new("fixtures.flint.dev", "v1", "gadgets");
    let a = FakeApiServer::start_blocking(Options::default())?;
    let b = FakeApiServer::start_blocking(Options {
        hidden: vec![gadgets.clone()],
        ..Options::default()
    })?;
    let expected = a.rows(&gadgets).len();
    ensure!(expected > 0, "the fixtures hold gadgets");
    let scratch = flint_e2e::scratch::Scratch::new()?;
    let kubeconfig = scratch.path().join("kubeconfig");
    write_kubeconfigs(&kubeconfig, &[(&a, A, None), (&b, B, None)])?;

    let app = App::launch(AppOptions::new(&kubeconfig))?;
    connect(&app, A)?;
    open_together(&app, &[A, B])?;
    app.open_view("gadgets.fixtures.flint.dev")?;
    app.wait_for(
        "/sets/open/list/kind",
        Equals("gadgets.fixtures.flint.dev".into()),
    )?;
    app.wait_for(ROWS, Test::at_least(expected as u32))?;

    // B's own answer, not its loading banner, with A's rows in the same snapshot.
    let state = app.wait_for(
        "/sets/open/list/banners",
        Any(json!({"id": format!("list-member-{B}"), "text": format!("{B}: unavailable")})),
    )?;
    let rows = shown(&state);
    assert_eq!(rows.len(), expected, "{rows:?}");
    ensure!(rows.iter().all(|(cluster, _)| cluster == A), "{rows:?}");

    app.quit()?;
    drain_after_quit();
    assert_no_writes(&[&a, &b])?;
    Ok(())
}

// covers UP-84
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_84_an_unreachable_member_does_not_block_the_others() -> Result<()> {
    let TwoClusters { a, b, scratch } = TwoClusters::start()?;
    seed_pods(&a, &[("still-here", "Running")]);
    let kubeconfig = scratch.path().join("kubeconfig");
    // B stays bound, so nothing else can take its port, and drops every connection.
    b.inject(Fault::drop_connection());

    let app = App::launch(AppOptions::new(&kubeconfig))?;
    connect(&app, A)?;
    open_together(&app, &[A, B])?;

    // A's list loads independently of B's failure, so wait for both rather than
    // reading A's rows at the instant B's note appears.
    let state = until(
        "a failure or reconnecting note on b after an attempt, and a's pods shown",
        || {
            let attempted = !b.arrivals().is_empty();
            (attempted, app.state().unwrap_or(Value::Null))
        },
        |(attempted, state)| {
            let note = text(&member(state, B)["note"]);
            *attempted
                && (note.starts_with("Failed") || note.starts_with("Reconnecting"))
                && shown(state) == pairs(&[(A, "still-here")])
        },
    )?
    .1;
    assert_eq!(member(&state, A)["note"], Value::Null);
    assert_eq!(shown(&state), pairs(&[(A, "still-here")]));

    app.quit()?;
    drain_after_quit();
    assert_no_writes(&[&a, &b])?;
    Ok(())
}
