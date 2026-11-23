//! View tests for cluster sets: choosing clusters, the combined list, a row's action reaching
//! only its own cluster, and Cmd-F. Rows are fed by hand; no write leaves the process.

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flint_core::handle::{MemberReport, MemberStatus};
use flint_core::settings::ClusterPin;
use flint_core::{
    ClusterSummary, ConnectionState, Event, KindId, KindSnapshot, NavEntry, SafeErrorText,
    ServerVersion, SyncState,
};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::super::Workspace;
use super::super::actions::resource_actions_tests::LocalApi;
use super::{SettingsDir, member_refusal};
use crate::ui::confirm;
use crate::ui::list::combined::row_id;
use crate::ui::resource_actions::{ResourceAction, available, request};
use crate::ui::support_tests::{
    CURRENT_GENERATION, TestWorkspace, WAIT, discovery, kubeconfig_yaml, objects, open_workspace,
    open_workspace_with, standard_sections,
};

fn pods_of(cluster: &str, rows: &[(&str, &str, i64)]) -> Arc<KindSnapshot> {
    let mut snapshot = (*objects(flint_core::KindId::pods(), rows, SyncState::Synced, 1)).clone();
    snapshot.cluster = cluster.into();
    Arc::new(snapshot)
}

fn pick(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str) {
    t.dispatch(
        cx,
        "cluster_sets::TogglePickedCluster",
        json!({ "cluster": cluster }),
    )
    .unwrap();
}

/// Opens both test clusters as a set, with a pod called `web` in each, and stops the live feeds.
async fn open_both(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.dispatch(cx, "cluster_sets::ToggleClusterPicking", Value::Null)
        .unwrap();
    pick(cx, t, "kind-alpha");
    pick(cx, t, "kind-beta");
    t.dispatch(cx, "cluster_sets::OpenPickedClusters", Value::Null)
        .unwrap();
    t.wait_for_element(cx, "cluster-set-view").await;
    let window = t.window;
    let workspace = t.workspace.clone();
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.sets.running.as_mut().unwrap()._feeds.clear();
            ws.feed_member(pods_of("kind-alpha", &[("default", "web", 1)]), window, cx);
            ws.feed_member(pods_of("kind-beta", &[("default", "web", 2)]), window, cx);
        })
    })
    .unwrap();
    let row = gpui_kit::SharedString::from(row_id("kind-beta", "uid-web"));
    cx.wait_for(t.window, WAIT, move |window, _| {
        window.try_find(row.clone()).is_some()
    })
    .await;
}

fn connect(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str) {
    t.feed(
        cx,
        Event::ConnectionChanged {
            cluster: cluster.into(),
            state: ConnectionState::Connected(ServerVersion {
                git_version: "v1.33.1".into(),
                platform: "linux/arm64".into(),
            }),
        },
    );
    let mut found = discovery(standard_sections());
    for kind in &mut found.kinds {
        kind.verbs.push("delete".into());
    }
    t.feed(
        cx,
        Event::DiscoveryChanged {
            cluster: cluster.into(),
            generation: CURRENT_GENERATION,
            discovery: Some(Arc::new(found)),
            error: None,
        },
    );
}

fn select_row(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str) {
    t.dispatch(
        cx,
        "combined_list::SelectCombinedRow",
        json!({ "cluster": cluster, "uid": "uid-web" }),
    )
    .unwrap();
}

fn detail_cluster(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<String> {
    t.workspace.read_with(cx, |ws, cx| {
        ws.detail_view()
            .map(|d| d.read(cx).object().cluster.clone())
    })
}

#[gpui_kit::test]
async fn choosing_clusters_never_connects_and_is_limited(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    assert!(
        t.dispatch(cx, "cluster_sets::OpenPickedClusters", Value::Null)
            .is_err(),
        "nothing chosen"
    );
    t.dispatch(cx, "cluster_sets::ToggleClusterPicking", Value::Null)
        .unwrap();
    pick(cx, &t, "kind-alpha");
    assert!(
        t.dispatch(cx, "cluster_sets::OpenPickedClusters", Value::Null)
            .is_err(),
        "one is not a set"
    );
    pick(cx, &t, "kind-beta");
    pick(cx, &t, "kind-beta");
    assert_eq!(t.connection_attempts("kind-alpha"), 0);
    assert!(!t.server_was_contacted(), "choosing connects to nothing");
}

#[gpui_kit::test]
async fn a_saved_set_opens_its_combined_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    cx.update(|cx| cx.set_global(SettingsDir(dir.path().to_owned())));
    t.dispatch(cx, "cluster_sets::ToggleClusterPicking", Value::Null)
        .unwrap();
    pick(cx, &t, "kind-alpha");
    pick(cx, &t, "kind-beta");
    t.dispatch(
        cx,
        "cluster_sets::SaveClusterSet",
        json!({ "name": "both" }),
    )
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        !workspace.read(cx).sets.saved.is_empty()
    })
    .await;
    assert!(t.settings_toml().contains("both"));
    t.dispatch(
        cx,
        "cluster_sets::OpenClusterSet",
        json!({ "name": "both" }),
    )
    .unwrap();
    t.wait_for_element(cx, "cluster-set-view").await;
    t.dispatch(cx, "cluster_sets::CloseClusterSet", Value::Null)
        .unwrap();
    assert!(t.find(cx, "cluster-set-view").is_none());
}

/// Forwards to `upstream`, counting the PATCH requests that pass.
fn counting_proxy(upstream: &str) -> (String, Arc<AtomicUsize>) {
    let upstream = upstream.trim_start_matches("http://").to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let patches = Arc::new(AtomicUsize::new(0));
    let counted = patches.clone();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(server) = TcpStream::connect(&upstream) else {
                continue;
            };
            let (mut from_client, mut to_server) = (client.try_clone().unwrap(), server);
            let (mut from_server, mut to_client) = (to_server.try_clone().unwrap(), client);
            let counted = counted.clone();
            std::thread::spawn(move || {
                let mut buffer = [0; 16 * 1024];
                while let Ok(read @ 1..) = from_client.read(&mut buffer) {
                    if buffer[..read].starts_with(b"PATCH ") {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                    if to_server.write_all(&buffer[..read]).is_err() {
                        break;
                    }
                }
                let _ = to_server.shutdown(Shutdown::Write);
            });
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut from_server, &mut to_client);
                let _ = to_client.shutdown(Shutdown::Write);
            });
        }
    });
    (url, patches)
}

fn deployments_entry() -> NavEntry {
    NavEntry {
        id: KindId::new("apps", "deployments"),
        label: "Deployments".into(),
        kind: "Deployment".into(),
        api_version: "apps/v1".into(),
        namespaced: true,
        listable: true,
        watchable: true,
    }
}

// Two clusters each hold a Deployment `web` with one uid: the write reaches only the row's.
#[gpui_kit::test]
async fn a_rows_action_writes_only_to_its_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (alpha_api, beta_api) = (LocalApi::start(&[]), LocalApi::start(&[]));
    let (alpha_url, alpha_patches) = counting_proxy(&alpha_api.url);
    let (beta_url, beta_patches) = counting_proxy(&beta_api.url);
    let t = open_workspace_with(cx, dir.path(), move |dir, _| {
        let path = dir.join("kubeconfig");
        let contexts = [("kind-alpha", &*alpha_url), ("kind-beta", &*beta_url)];
        std::fs::write(&path, kubeconfig_yaml(&contexts)).unwrap();
        vec![path]
    })
    .await;
    t.dispatch(cx, "cluster_sets::ToggleClusterPicking", Value::Null)
        .unwrap();
    pick(cx, &t, "kind-alpha");
    pick(cx, &t, "kind-beta");
    t.dispatch(cx, "cluster_sets::OpenPickedClusters", Value::Null)
        .unwrap();
    for cluster in ["kind-alpha", "kind-beta"] {
        t.wait_for_state(cx, cluster, |s| matches!(s, ConnectionState::Connected(_)))
            .await;
        let model = t.model.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| {
            model
                .read(cx)
                .discovered(cluster)
                .is_some_and(|d| d.discovery.is_some())
        })
        .await;
    }
    let deployments = KindId::new("apps", "deployments");
    let (workspace, window) = (t.workspace.clone(), t.window);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            assert!(ws.show_kind_in_set(&deployments_entry(), window, cx));
            ws.sets.running.as_mut().unwrap()._feeds.clear();
            for cluster in ["kind-alpha", "kind-beta"] {
                let rows = [("demo", "web", 2)];
                let listed = objects(deployments.clone(), &rows, SyncState::Synced, 1);
                let mut listed = (*listed).clone();
                listed.cluster = cluster.into();
                ws.feed_member(Arc::new(listed), window, cx);
            }
        })
    })
    .unwrap();
    let row = gpui_kit::SharedString::from(row_id("kind-beta", "uid-web"));
    cx.wait_for(t.window, WAIT, move |window, _| {
        window.try_find(row.clone()).is_some()
    })
    .await;
    select_row(cx, &t, "kind-beta");
    let detail = t.detail(cx).expect("the row's detail");
    assert_eq!(detail_cluster(cx, &t).as_deref(), Some("kind-beta"));
    cx.wait_for(t.window, WAIT, move |_, cx| {
        detail.read(cx).replicas() == Some(2)
    })
    .await;

    let workspace = t.workspace.clone();
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            assert_eq!(available(ws, ResourceAction::RolloutRestart, cx), Ok(()));
            request(ws, ResourceAction::RolloutRestart, window, cx);
        })
    })
    .unwrap();
    cx.wait_for(t.window, WAIT, |window, _| window.find_all("ok").len() == 1)
        .await;
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    let shown = cx
        .update_window(t.window, |_, window, cx| confirm::shown(window, cx))
        .unwrap()
        .expect("the review");
    assert_eq!(shown.title, "Restart web on kind-beta?");
    assert_eq!(beta_patches.load(Ordering::SeqCst), 0, "nothing ran yet");
    t.click(cx, "ok");
    let written = beta_patches.clone();
    cx.wait_for(t.window, WAIT, move |_, _| {
        written.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(alpha_patches.load(Ordering::SeqCst), 0, "the other cluster");
}

#[gpui_kit::test]
async fn a_protected_members_row_refuses_its_actions(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.toml"),
        "protect_local_clusters = true\n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    connect(cx, &t, "kind-alpha");
    connect(cx, &t, "kind-beta");
    open_both(cx, &t).await;
    select_row(cx, &t, "kind-beta");
    assert_eq!(detail_cluster(cx, &t).as_deref(), Some("kind-beta"));
    let why = t.workspace.read_with(cx, |ws, cx| {
        available(ws, ResourceAction::Delete, cx).unwrap_err()
    });
    assert!(why.contains("kind-beta"), "{why}");
}

#[gpui_kit::test]
async fn a_member_needing_sign_in_is_shown_and_its_row_closes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    connect(cx, &t, "kind-alpha");
    connect(cx, &t, "kind-beta");
    open_both(cx, &t).await;
    select_row(cx, &t, "kind-beta");
    assert_eq!(detail_cluster(cx, &t).as_deref(), Some("kind-beta"));
    let report = MemberReport {
        cluster: "kind-beta".into(),
        status: MemberStatus::SignInNeeded {
            reason: SafeErrorText::reduced("expired"),
        },
        paused: false,
    };
    t.workspace.update(cx, |ws, cx| {
        ws.set_member_reports(vec![report], cx);
    });
    cx.run_until_parked();
    assert_eq!(detail_cluster(cx, &t), None, "no action stays on offer");
    let label = t.label(cx, "set-member-kind-beta").unwrap();
    assert!(label.ends_with("Sign-in needed"), "{label}");
}

#[gpui_kit::test]
async fn a_member_that_changed_loses_its_rows_and_offers_no_action(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    connect(cx, &t, "kind-alpha");
    connect(cx, &t, "kind-beta");
    open_both(cx, &t).await;
    select_row(cx, &t, "kind-beta");
    assert_eq!(detail_cluster(cx, &t).as_deref(), Some("kind-beta"));
    let header = t.label(cx, "detail-cluster").expect("the exact context");
    assert!(header.contains("kind-beta"), "{header}");

    let (workspace, window) = (t.workspace.clone(), t.window);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.discard_member("kind-beta", "kind-beta changed".into(), cx);
            ws.check_set_row(cx);
            // A snapshot still on its way is not taken for the member's rows.
            ws.feed_member(pods_of("kind-beta", &[("default", "web", 3)]), window, cx);
        })
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(detail_cluster(cx, &t), None, "its row closed");
    let (rows, refused) = t.workspace.read_with(cx, |ws, cx| {
        let run = ws.sets.running.as_ref().unwrap();
        let beta = run.snapshots.iter().find(|s| s.cluster == "kind-beta");
        (
            beta.unwrap().rows.len(),
            ws.session_member("kind-beta", cx).unwrap_err(),
        )
    });
    assert_eq!(rows, 0);
    assert!(refused.contains("changed"), "{refused}");
    let label = t.label(cx, "set-member-kind-beta").unwrap();
    assert!(label.ends_with("Changed"), "{label}");
}

#[gpui_kit::test]
async fn a_row_tab_does_not_restart_once_its_context_moved(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    connect(cx, &t, "kind-alpha");
    connect(cx, &t, "kind-beta");
    open_both(cx, &t).await;
    let (workspace, window) = (t.workspace.clone(), t.window);
    let request = crate::ui::log_view::LogRequest {
        cluster: "kind-beta".into(),
        subject: flint_core::LogSubject::Pod(flint_core::LogTarget {
            namespace: "default".into(),
            pod: "web".into(),
            container: None,
        }),
        previous: false,
    };
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.open_logs(request, true, window, cx))
    })
    .unwrap();
    let (id, core) = t.workspace.read_with(cx, |ws, cx| {
        let id = ws.dock.read(cx).tab_ids()[0];
        assert!(ws.sets.tab_pin(id).is_some(), "the tab keeps the row's pin");
        (id, ws.core(cx).clone())
    });

    std::fs::write(
        dir.path().join("kubeconfig"),
        kubeconfig_yaml(&[
            ("kind-alpha", "https://127.0.0.1:1"),
            ("kind-beta", "https://replacement.invalid"),
        ]),
    )
    .unwrap();
    core.reload_kubeconfig().await.unwrap();
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.restart_tab(id, window, cx))
    })
    .unwrap();
    let waiting = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        waiting.read(cx).action_error().is_some()
    })
    .await;
    t.workspace.read_with(cx, |ws, cx| {
        assert_eq!(ws.dock.read(cx).tab_ids(), vec![id], "nothing restarted");
        let why = ws.action_error().unwrap();
        assert!(why.contains("changed"), "{why}");
    });
}

#[gpui_kit::test]
async fn a_selected_row_comes_back_in_front_of_the_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    connect(cx, &t, "kind-alpha");
    connect(cx, &t, "kind-beta");
    open_both(cx, &t).await;
    select_row(cx, &t, "kind-beta");
    t.workspace.update(cx, |ws, _| ws.list_in_front = true);
    select_row(cx, &t, "kind-beta");
    cx.run_until_parked();
    assert!(
        t.workspace.read_with(cx, |ws, _| !ws.list_in_front),
        "a repeated click reopens it"
    );

    t.workspace.update(cx, |ws, _| ws.list_in_front = true);
    let (workspace, window) = (t.workspace.clone(), t.window);
    cx.update_window(window, |_, window, cx| {
        let list = workspace
            .read(cx)
            .sets
            .running
            .as_ref()
            .unwrap()
            .list
            .clone();
        let focus = list.read(cx).focus_handle().clone();
        window.focus(&focus, cx);
    })
    .unwrap();
    t.dispatch(cx, "resource_list::OpenSelectedRow", Value::Null)
        .unwrap();
    cx.run_until_parked();
    assert!(
        t.workspace.read_with(cx, |ws, _| !ws.list_in_front),
        "Enter reopens it"
    );
    assert_eq!(detail_cluster(cx, &t).as_deref(), Some("kind-beta"));
}

#[gpui_kit::test]
async fn cmd_f_reaches_the_combined_filter(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_both(cx, &t).await;
    t.focus_workspace(cx);
    assert!(
        t.available(cx).contains(&"resource_list::FocusFilter"),
        "offered with only a combined list open"
    );
    t.press_key_of(cx, "resource_list::FocusFilter");
    let (workspace, window) = (t.workspace.clone(), t.window);
    let focused = cx
        .update_window(window, |_, window, cx| {
            let ws = workspace.read(cx);
            let list = ws.sets.running.as_ref().unwrap().list.read(cx);
            list.filter_handle(cx).is_focused(window)
        })
        .unwrap();
    assert!(focused, "focus is in the combined list's filter");
}

#[test]
fn a_member_is_refused_unless_it_is_the_pinned_confirmed_cluster() {
    let pin = ClusterPin::new("prod", "https://a.example");
    let summary = |server: &str, confirmed: bool| ClusterSummary {
        info: flint_core::kubeconfig::ContextInfo {
            name: "prod".into(),
            cluster: None,
            user: None,
            namespace: None,
            server: Some(server.into()),
            proxy_url: None,
            source: "kubeconfig".into(),
        },
        state: ConnectionState::Disconnected,
        generation: 1,
        definition: 1,
        endpoint: 1,
        confirmed,
    };
    let report = |paused, status| MemberReport {
        cluster: "prod".into(),
        status,
        paused,
    };
    let current = summary("https://a.example", true);
    assert_eq!(member_refusal(&pin, Some(&current), None, "prod"), None);
    let live = report(false, MemberStatus::Live);
    assert_eq!(
        member_refusal(&pin, Some(&current), Some(&live), "prod"),
        None
    );
    for (summary, report, says) in [
        (None, None, "no longer"),
        (
            Some(summary("https://a.example", false)),
            None,
            "provisional",
        ),
        (
            Some(summary("https://b.example", true)),
            None,
            "another server",
        ),
        (
            Some(current.clone()),
            Some(report(true, MemberStatus::Live)),
            "paused",
        ),
        (
            Some(current.clone()),
            Some(report(
                false,
                MemberStatus::SignInNeeded {
                    reason: SafeErrorText::reduced("expired"),
                },
            )),
            "sign-in",
        ),
    ] {
        let why = member_refusal(&pin, summary.as_ref(), report.as_ref(), "prod").unwrap();
        assert!(why.contains(says), "{why}");
    }
}

#[test]
fn every_set_and_combined_action_is_bound() {
    use crate::ui::list::combined::CombinedResourceList;
    use crate::ui::registry::{Scope, tests::assert_binds_scope};
    assert_binds_scope(Scope::ClusterSets, Workspace::bind_set_actions);
    assert_binds_scope(Scope::CombinedList, CombinedResourceList::bind_actions);
}
