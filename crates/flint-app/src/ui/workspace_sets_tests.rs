//! View tests for cluster sets: choosing clusters, the combined list, a row's action reaching
//! only its own cluster, and Cmd-F. Rows are fed by hand; no write leaves the process.

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flint_core::handle::{MemberReport, MemberStatus, OpenedSet};
use flint_core::protection::WriteRefusal;
use flint_core::settings::ClusterPin;
use flint_core::write::{WriteError, WriteProposal, WriteReview};
use flint_core::{
    ClusterSummary, ConnectionState, Event, KindId, KindSnapshot, NavEntry, SafeErrorText,
    ServerVersion, SyncState,
};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::super::Workspace;
use super::super::actions::resource_actions_tests::LocalApi;
use super::super::unapplied_tests::{answered, asked, has_edit, type_edit};
use super::{
    ClusterSet, CombinedListEvent, PendingOpen, SettingsDir, click_chooses, member_refusal,
    rename_problem,
};
use crate::ui::confirm;
use crate::ui::fleet_plan::FleetChoice;
use crate::ui::fleet_view;
use crate::ui::list::combined::row_id;
use crate::ui::resource_actions::{ResourceAction, available, request, target_of};
use crate::ui::support_tests::{
    CURRENT_GENERATION, TestWorkspace, WAIT, discovery, kubeconfig_yaml, objects,
    open_workspace_with, standard_sections,
};

fn pods_of(cluster: &str, rows: &[(&str, &str, i64)]) -> Arc<KindSnapshot> {
    let mut snapshot = (*objects(flint_core::KindId::pods(), rows, SyncState::Synced, 1)).clone();
    snapshot.cluster = cluster.into();
    Arc::new(snapshot)
}

/// The workspace over `kind-alpha` and `kind-beta`, both on the server that accepts connections
/// and never answers. The tests feed each member's state by hand; a server refusing connections
/// would let the core's own attempt fail and replace a fed `Connected` at a time that depends on
/// the machine's speed.
async fn open_quiet_workspace(cx: &mut TestAppContext, dir: &std::path::Path) -> TestWorkspace {
    open_workspace_with(cx, dir, |dir, servers| {
        let path = dir.join("kubeconfig");
        let contexts = [
            ("kind-alpha", servers.silent.as_str()),
            ("kind-beta", servers.silent.as_str()),
        ];
        std::fs::write(&path, kubeconfig_yaml(&contexts)).unwrap();
        vec![path]
    })
    .await
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
    let t = open_quiet_workspace(cx, dir.path()).await;
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
    let t = open_quiet_workspace(cx, dir.path()).await;
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

/// Forwards to `upstream`, counting the PATCH, PUT and DELETE requests that pass.
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
                    let sent = &buffer[..read];
                    if [&b"PATCH "[..], b"PUT ", b"DELETE "]
                        .iter()
                        .any(|verb| sent.starts_with(verb))
                    {
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

/// A set of two fake API servers, each with a Deployment `web`, and `kind-beta`'s row open.
struct OpenRow {
    t: TestWorkspace,
    alpha_url: String,
    beta_url: String,
    alpha_writes: Arc<AtomicUsize>,
    beta_writes: Arc<AtomicUsize>,
    _apis: (LocalApi, LocalApi),
}

async fn open_beta_row(cx: &mut TestAppContext, dir: &std::path::Path) -> OpenRow {
    open_beta_row_answering(cx, dir, &[]).await
}

/// [`open_beta_row`], with `kind-alpha` answering its writes with `alpha_statuses` in turn.
async fn open_beta_row_answering(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    alpha_statuses: &[u16],
) -> OpenRow {
    let (alpha_api, beta_api) = (LocalApi::start(alpha_statuses), LocalApi::start(&[]));
    let (alpha_url, alpha_writes) = counting_proxy(&alpha_api.url);
    let (beta_url, beta_writes) = counting_proxy(&beta_api.url);
    let urls = (alpha_url.clone(), beta_url.clone());
    let t = open_workspace_with(cx, dir, move |dir, _| {
        let path = dir.join("kubeconfig");
        let contexts = [("kind-alpha", &*urls.0), ("kind-beta", &*urls.1)];
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
    OpenRow {
        t,
        alpha_url,
        beta_url,
        alpha_writes,
        beta_writes,
        _apis: (alpha_api, beta_api),
    }
}

/// Offers a restart of the open row and waits for its confirmation.
async fn offer_restart(cx: &mut TestAppContext, t: &TestWorkspace) {
    let (workspace, window) = (t.workspace.clone(), t.window);
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
}

/// Points `kind-beta` at a new fake API server. Returns its count of writes.
async fn repoint_beta(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    row: &OpenRow,
) -> (String, Arc<AtomicUsize>, LocalApi) {
    let api = LocalApi::start(&[]);
    let (url, writes) = counting_proxy(&api.url);
    let contexts = [("kind-alpha", &*row.alpha_url), ("kind-beta", &*url)];
    std::fs::write(dir.join("kubeconfig"), kubeconfig_yaml(&contexts)).unwrap();
    let core = row.t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());
    core.reload_kubeconfig().await.unwrap();
    cx.run_until_parked();
    (url, writes, api)
}

fn is_moved(result: Result<WriteReview, WriteError>) -> bool {
    matches!(result, Err(WriteError::Refused(WriteRefusal::Moved { .. })))
}

// Two clusters each hold a Deployment `web` with one uid: the write reaches only the row's.
#[gpui_kit::test]
async fn a_rows_action_writes_only_to_its_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let row = open_beta_row(cx, dir.path()).await;
    let t = &row.t;
    offer_restart(cx, t).await;
    let shown = cx
        .update_window(t.window, |_, window, cx| confirm::shown(window, cx))
        .unwrap()
        .expect("the review");
    assert_eq!(shown.title, "Restart web on kind-beta?");
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 0, "nothing ran yet");
    t.click(cx, "ok");
    let written = row.beta_writes.clone();
    cx.wait_for(t.window, WAIT, move |_, _| {
        written.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(
        row.alpha_writes.load(Ordering::SeqCst),
        0,
        "the other cluster"
    );
}

// Re-pointed after the confirmation opened: Confirm reaches neither server.
#[gpui_kit::test]
async fn a_row_repointed_before_confirm_writes_nowhere(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let row = open_beta_row(cx, dir.path()).await;
    let t = &row.t;
    offer_restart(cx, t).await;
    let (_url, replacement, _api) = repoint_beta(cx, dir.path(), &row).await;
    if cx
        .update_window(t.window, |_, window, _| window.find_all("ok").len())
        .unwrap()
        == 1
    {
        t.click(cx, "ok");
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(replacement.load(Ordering::SeqCst), 0, "the replacement");
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 0, "the old server");
    assert_eq!(row.alpha_writes.load(Ordering::SeqCst), 0, "the other one");
}

// The row's target keeps its pin whatever the context shows, and the core refuses it, for an
// action and for a YAML edit alike, before any request is written.
#[gpui_kit::test]
async fn a_rows_writes_stay_pinned_and_the_gate_refuses_them(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let row = open_beta_row(cx, dir.path()).await;
    let t = &row.t;
    let detail = t.detail(cx).expect("the row's detail");
    let (url, replacement, _api) = repoint_beta(cx, dir.path(), &row).await;
    // The row closes on the change; a write already on its way still holds this view's pin.
    let (target, core) = t.workspace.update(cx, |ws, cx| {
        ws.detail = Some(detail.clone());
        detail.update(cx, |d, _| d.set_server(Some(url.clone())));
        (
            target_of(ws, cx).unwrap(),
            detail.read(cx).write_core().clone(),
        )
    });
    assert_eq!(target.server.as_deref(), Some(&*row.beta_url));
    detail.read_with(cx, |d, _| {
        assert_eq!(d.edit_server(), Some(&*row.beta_url), "a YAML edit too");
    });
    let action = core.review_write(target.clone(), WriteProposal::RolloutRestart);
    assert!(is_moved(action.await), "a resource action");
    let edit = WriteProposal::ApplyYaml {
        yaml: "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n".to_owned(),
    };
    assert!(
        is_moved(core.review_write(target, edit).await),
        "a YAML edit"
    );
    assert_eq!(replacement.load(Ordering::SeqCst), 0, "the replacement");
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 0, "the old server");
}

// A Scale dialog left open while the context goes away and comes back on the same URL with
// another user: confirming reaches no server, and the core refuses the row's handle.
#[gpui_kit::test]
async fn a_stale_dialog_writes_nothing_after_away_and_back_with_another_user(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let row = open_beta_row(cx, dir.path()).await;
    let t = &row.t;
    let detail = t.detail(cx).expect("the row's detail");
    let (target, bound) = t.workspace.read_with(cx, |ws, cx| {
        (
            target_of(ws, cx).unwrap(),
            detail.read(cx).write_core().clone(),
        )
    });
    let (workspace, window) = (t.workspace.clone(), t.window);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            request(ws, ResourceAction::Scale, window, cx);
        })
    })
    .unwrap();
    cx.wait_for(t.window, WAIT, |window, _| window.find_all("ok").len() == 1)
        .await;
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();

    let (_url, replacement, _api) = repoint_beta(cx, dir.path(), &row).await;
    let contexts = [
        ("kind-alpha", &*row.alpha_url),
        ("kind-beta", &*row.beta_url),
    ];
    let other_user = kubeconfig_yaml(&contexts).replace("token: t", "token: other");
    std::fs::write(dir.path().join("kubeconfig"), other_user).unwrap();
    let core = t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());
    core.reload_kubeconfig().await.unwrap();
    cx.run_until_parked();
    // Back and connected with the new user, so only the admitted definition can refuse it.
    t.model
        .update(cx, |model, _| model.activate("kind-beta".into()));
    t.wait_for_state(cx, "kind-beta", |s| {
        matches!(s, ConnectionState::Connected(_))
    })
    .await;
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .discovered("kind-beta")
            .is_some_and(|d| d.discovery.is_some())
    })
    .await;

    if cx
        .update_window(t.window, |_, window, _| window.find_all("ok").len())
        .unwrap()
        == 1
    {
        t.click(cx, "ok");
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    let refused = bound
        .review_write(target, WriteProposal::Scale { to: 7 })
        .await;
    assert!(
        matches!(refused, Err(WriteError::ConnectionChanged)),
        "{:?}",
        refused.map(|_| ())
    );
    assert_eq!(
        row.beta_writes.load(Ordering::SeqCst),
        0,
        "the row's server"
    );
    assert_eq!(replacement.load(Ordering::SeqCst), 0, "the replacement");
    assert_eq!(row.alpha_writes.load(Ordering::SeqCst), 0, "the other one");
}

#[gpui_kit::test]
async fn a_protected_members_row_refuses_its_actions(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.toml"),
        "protect_local_clusters = true\n",
    )
    .unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
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

// A member whose definition can't be confirmed keeps its row to read, says writes are paused,
// and offers none of them, however it is unlocked. They come back once it is confirmed.
#[gpui_kit::test]
async fn a_member_with_paused_writes_keeps_its_row_and_offers_no_write(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    connect(cx, &t, "kind-alpha");
    connect(cx, &t, "kind-beta");
    open_both(cx, &t).await;
    select_row(cx, &t, "kind-beta");
    let detail = t.detail(cx).expect("the row's detail");
    let set_provisional = |provisional, cx: &mut TestAppContext| {
        t.model.update(cx, |model, cx| {
            let beta = model
                .clusters
                .iter_mut()
                .find(|c| c.info.name == "kind-beta")
                .unwrap();
            beta.provisional = provisional;
            cx.notify();
        });
        cx.run_until_parked();
    };
    set_provisional(true, cx);
    assert_eq!(
        member_note(cx, &t, "kind-beta").as_deref(),
        Some("Writes paused")
    );
    assert_eq!(detail_cluster(cx, &t).as_deref(), Some("kind-beta"));
    t.workspace.read_with(cx, |ws, cx| {
        let why = available(ws, ResourceAction::Delete, cx).unwrap_err();
        assert!(why.contains("paused"), "{why}");
        let edit = detail.read(cx).edit_blocked().unwrap();
        assert!(edit.contains("paused"), "{edit}");
        assert!(ws.session_member("kind-beta", cx).is_ok(), "reads go on");
    });
    let request = crate::ui::terminal_model::ShellRequest {
        cluster: "kind-beta".into(),
        namespace: "default".into(),
        pod: "web".into(),
        container: None,
    };
    let (workspace, window) = (t.workspace.clone(), t.window);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.open_shell(request, true, window, cx))
    })
    .unwrap();
    t.workspace.read_with(cx, |ws, cx| {
        assert!(ws.dock.read(cx).tab_ids().is_empty(), "no shell opened");
        let why = ws.action_error().unwrap();
        assert!(why.contains("paused"), "{why}");
    });

    set_provisional(false, cx);
    assert_eq!(member_note(cx, &t, "kind-beta"), None);
    t.workspace.read_with(cx, |ws, cx| {
        assert_eq!(ws.writes_paused_why("kind-beta", cx), None);
        assert!(
            !detail
                .read(cx)
                .edit_blocked()
                .is_some_and(|w| w.contains("paused"))
        );
    });
}

#[gpui_kit::test]
async fn a_member_needing_sign_in_is_shown_and_its_row_closes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
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
    assert_eq!(
        member_note(cx, &t, "kind-beta").as_deref(),
        Some("Sign-in needed")
    );
}

#[gpui_kit::test]
async fn a_member_that_changed_loses_its_rows_and_offers_no_action(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
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
    assert_eq!(member_note(cx, &t, "kind-beta").as_deref(), Some("Changed"));
}

#[gpui_kit::test]
async fn a_row_tab_does_not_restart_once_its_context_moved(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
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
        // The sweep or the restart's own check may say it first.
        let why = ws.action_error().unwrap();
        assert!(
            why.contains("changed") || why.contains("another server"),
            "{why}"
        );
    });
}

#[gpui_kit::test]
async fn a_selected_row_comes_back_in_front_of_the_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
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
    let t = open_quiet_workspace(cx, dir.path()).await;
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
        write_epoch: 0,
        confirmed,
        provisional: !confirmed,
        paused: false,
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
    // Unchanged while a file is unreadable: it keeps reading, and only its writes are paused.
    let writes_paused = ClusterSummary {
        provisional: true,
        ..current.clone()
    };
    assert_eq!(
        member_refusal(&pin, Some(&writes_paused), Some(&live), "prod"),
        None
    );
    // The summary says so before the next member report does.
    let paused = ClusterSummary {
        provisional: true,
        paused: true,
        ..current.clone()
    };
    let why = member_refusal(&pin, Some(&paused), None, "prod").unwrap();
    assert!(why.contains("paused"), "{why}");
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

/// Saves `kind-alpha` and `kind-beta` as the set `both`.
async fn save_both(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.dispatch(cx, "cluster_sets::ToggleClusterPicking", Value::Null)
        .unwrap();
    pick(cx, t, "kind-alpha");
    pick(cx, t, "kind-beta");
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
}

fn saved_names(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<String> {
    t.workspace.read_with(cx, |ws, _| {
        ws.sets.saved.iter().map(|s| s.name.clone()).collect()
    })
}

#[gpui_kit::test]
async fn a_saved_set_is_renamed_and_its_open_view_follows(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    cx.update(|cx| cx.set_global(SettingsDir(dir.path().to_owned())));
    save_both(cx, &t).await;
    t.dispatch(
        cx,
        "cluster_sets::OpenClusterSet",
        json!({ "name": "both" }),
    )
    .unwrap();
    t.wait_for_element(cx, "cluster-set-view").await;
    t.dispatch(
        cx,
        "cluster_sets::RenameClusterSet",
        json!({ "name": "both" }),
    )
    .unwrap();
    cx.run_until_parked();
    t.press(
        cx,
        if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        },
    );
    t.type_text(cx, "pair");
    t.click(cx, "ok");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace
            .read(cx)
            .sets
            .saved
            .iter()
            .any(|s| s.name == "pair")
    })
    .await;
    assert_eq!(saved_names(cx, &t), ["pair"]);
    assert!(t.settings_toml().contains("pair"));
    let (label, title) = t.workspace.read_with(cx, |ws, cx| {
        let run = ws.sets.running.as_ref().unwrap();
        (run.label.clone(), run.list.read(cx).label.clone())
    });
    assert_eq!((label.as_str(), title.as_str()), ("pair", "Pods · pair"));
}

/// Both clusters as a set with `kind-beta`'s `web` open and an edit typed.
async fn editing_a_row(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
) -> (
    TestWorkspace,
    gpui_kit::Entity<crate::ui::detail::DetailView>,
) {
    let t = open_quiet_workspace(cx, dir).await;
    connect(cx, &t, "kind-alpha");
    connect(cx, &t, "kind-beta");
    open_both(cx, &t).await;
    cx.update_window(t.window, |_, window, _| window.activate_window())
        .unwrap();
    select_row(cx, &t, "kind-beta");
    cx.run_until_parked();
    let detail = t.detail(cx).expect("the row's detail");
    type_edit(cx, &t, &detail, "default", "web");
    assert!(has_edit(cx, &detail));
    (t, detail)
}

// An edit typed while a set is being admitted survives the set opening.
#[gpui_kit::test]
async fn an_edit_typed_during_admission_is_kept(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing_a_row(cx, dir.path()).await;
    let (workspace, window) = (t.workspace.clone(), t.window);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            let members = ws.sets.running.as_ref().unwrap().members.clone();
            ws.sets.opening = Some(PendingOpen {
                token: 99,
                set_id: None,
            });
            let opened = OpenedSet {
                members,
                left_out: Vec::new(),
            };
            ws.finish_open(99, None, Ok(opened), window, cx);
        })
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(t.detail(cx).as_ref(), Some(&detail), "the same pane");
    assert!(has_edit(cx, &detail));
}

// Reopening the edited row after its label changed, or once it needs a sign-in, keeps the pane.
#[gpui_kit::test]
async fn reopening_the_edited_row_keeps_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (t, detail) = editing_a_row(cx, dir.path()).await;
    t.workspace.update(cx, |ws, cx| {
        let list = ws.sets.running.as_ref().unwrap().list.clone();
        list.update(cx, |list, _| list.label = "Pods · renamed".into());
    });
    cx.run_until_parked();
    let (workspace, window) = (t.workspace.clone(), t.window);
    let reopen = |cx: &mut TestAppContext| {
        let workspace = workspace.clone();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                let list = ws.sets.running.as_ref().unwrap().list.clone();
                let id = list.read(cx).selected().cloned().expect("the row");
                ws.on_combined_event(&list, &CombinedListEvent::Reopened(id), window, cx);
            })
        })
        .unwrap();
        cx.run_until_parked();
    };
    reopen(cx);
    assert_eq!(t.detail(cx).as_ref(), Some(&detail), "after a rename");
    assert!(!t.has_dialog(cx), "the same object: nothing to ask");

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
    reopen(cx);
    assert_eq!(t.detail(cx).as_ref(), Some(&detail), "needing a sign-in");
    assert!(has_edit(cx, &detail));
}

// Discard replays a saved set with its original members: the moved one is still marked.
#[gpui_kit::test]
async fn discarding_keeps_the_left_out_notice(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let row = open_beta_row(cx, dir.path()).await;
    let t = &row.t;
    let detail = t.detail(cx).expect("the row's detail");
    cx.update_window(t.window, |_, window, _| window.activate_window())
        .unwrap();
    type_edit(cx, t, &detail, "demo", "web");
    let (moved_to, _connections) = counting_listener();
    let contexts = [
        ("kind-alpha", &*row.alpha_url),
        ("kind-beta", &*row.beta_url),
        ("kind-gamma", &*moved_to),
    ];
    std::fs::write(dir.path().join("kubeconfig"), kubeconfig_yaml(&contexts)).unwrap();
    let core = t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());
    core.reload_kubeconfig().await.unwrap();
    cx.run_until_parked();
    let pins = vec![
        ClusterPin::new("kind-alpha".to_owned(), row.alpha_url.clone()),
        ClusterPin::new("kind-beta".to_owned(), row.beta_url.clone()),
        ClusterPin::new("kind-gamma".to_owned(), "https://old.invalid".to_owned()),
    ];
    t.workspace.update(cx, |ws, _| {
        ws.sets.saved = vec![ClusterSet::new("three", pins).unwrap()];
    });
    t.dispatch(
        cx,
        "cluster_sets::OpenClusterSet",
        json!({ "name": "three" }),
    )
    .unwrap();
    asked(cx, t).await;
    t.click(cx, "ok");
    // The set it replaces was never saved: that asks as well.
    asked_about_unsaved_set(cx, t).await;
    t.click(cx, "ok");
    answered(cx, t).await;
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace
            .read(cx)
            .sets
            .running
            .as_ref()
            .is_some_and(|run| run.set_id.is_some())
    })
    .await;
    let left_out = t.workspace.read_with(cx, |ws, _| {
        ws.sets.running.as_ref().unwrap().left_out.clone()
    });
    let left_out: Vec<&str> = left_out.iter().map(|m| m.context.as_str()).collect();
    assert_eq!(left_out, ["kind-gamma"]);
}

#[test]
fn a_rename_refuses_another_sets_name_but_keeps_its_own() {
    let pins = || {
        vec![
            ClusterPin::new("a".to_owned(), "https://a.example".to_owned()),
            ClusterPin::new("b".to_owned(), "https://b.example".to_owned()),
        ]
    };
    let saved = [
        ClusterSet::new("prod-eu", pins()).unwrap(),
        ClusterSet::new("staging", pins()).unwrap(),
    ];
    let id = saved[0].id.clone();
    assert_eq!(rename_problem(&saved, &id, "PROD_EU"), None);
    assert_eq!(rename_problem(&saved, &id, "other"), None);
    assert!(rename_problem(&saved, &id, "Staging").is_some());
    assert!(rename_problem(&saved, &id, "   ").is_some());
}

fn server_of(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str) -> String {
    t.workspace.read_with(cx, |ws, cx| {
        let summary = ws.cluster_summary(cluster, cx).unwrap();
        summary.info.server.clone().unwrap()
    })
}

#[gpui_kit::test]
async fn deleting_the_open_set_closes_it_and_stops_the_core(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    cx.update(|cx| cx.set_global(SettingsDir(dir.path().to_owned())));
    save_both(cx, &t).await;
    t.dispatch(
        cx,
        "cluster_sets::OpenClusterSet",
        json!({ "name": "both" }),
    )
    .unwrap();
    t.wait_for_element(cx, "cluster-set-view").await;
    let id = t
        .workspace
        .read_with(cx, |ws, _| ws.sets.saved[0].id.clone());
    let window = t.window;
    let workspace = t.workspace.clone();
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.delete_set(id, window, cx))
    })
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        !workspace.read(cx).sets.is_open()
    })
    .await;
    let core = t
        .workspace
        .read_with(cx, |ws, cx| ws.model.read(cx).core().clone());
    let members = core.running_set().await;
    assert!(members.is_empty(), "the core runs nothing: {members:?}");
}

#[gpui_kit::test]
async fn deleting_a_set_stops_its_open_on_the_way(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    cx.update(|cx| cx.set_global(SettingsDir(dir.path().to_owned())));
    save_both(cx, &t).await;
    let id = t
        .workspace
        .read_with(cx, |ws, _| ws.sets.saved[0].id.clone());
    let window = t.window;
    let workspace = t.workspace.clone();
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            let set = ws.sets.saved[0].clone();
            let saved = super::SavedOpening {
                name: set.name,
                id: set.id,
            };
            ws.open_set(set.members, Some(saved), window, cx);
            assert!(ws.sets.opening.is_some(), "still on its way");
            ws.close_deleted_set(&id, window, cx);
            assert!(ws.sets.opening.is_none(), "no longer wanted");
        })
    })
    .unwrap();
    let core = t
        .workspace
        .read_with(cx, |ws, cx| ws.model.read(cx).core().clone());
    // The core answers on its own thread, and only that answer can stop what it admitted.
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        cx.run_until_parked();
        let members = core.running_set().await;
        if members.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the core runs nothing: {members:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(t.workspace.read_with(cx, |ws, _| !ws.sets.is_open()));
}

// Chosen on one server, the context moves: neither saving nor opening pins the new server.
#[gpui_kit::test]
async fn a_choice_stays_on_the_server_it_was_made_on(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    cx.update(|cx| cx.set_global(SettingsDir(dir.path().to_owned())));
    let alpha = server_of(cx, &t, "kind-alpha");
    pick(cx, &t, "kind-alpha");
    pick(cx, &t, "kind-beta");
    let held = t.workspace.read_with(cx, |ws, _| ws.sets.picked.clone());
    assert_eq!(held[0], ClusterPin::new("kind-alpha", alpha));

    // The choice as it would stand had the context moved since it was made.
    t.workspace.update(cx, |ws, _| {
        ws.sets.picked[1].server = "https://chosen-before.invalid".to_owned();
    });
    let pinned = t.workspace.read_with(cx, |ws, cx| ws.picked_pins(cx));
    assert_eq!(pinned[1].server, "https://chosen-before.invalid");

    t.dispatch(
        cx,
        "cluster_sets::SaveClusterSet",
        json!({ "name": "moved" }),
    )
    .unwrap();
    cx.run_until_parked();
    let (saved, error) = t.workspace.read_with(cx, |ws, _| {
        (ws.sets.saved.is_empty(), ws.sets.error.clone())
    });
    assert!(saved, "nothing is saved");
    let error = error.expect("says why");
    assert!(error.contains("another server"), "{error}");

    t.dispatch(cx, "cluster_sets::OpenPickedClusters", Value::Null)
        .unwrap();
    wait_until_answered(cx, &t).await;
    let (open, error) = t
        .workspace
        .read_with(cx, |ws, _| (ws.sets.is_open(), ws.sets.error.clone()));
    assert!(!open, "one member left is too few");
    assert!(error.expect("says why").contains("kind-beta"));
    assert!(!t.server_was_contacted(), "nothing started");
}

#[gpui_kit::test]
async fn deleting_a_set_waits_for_the_confirmation(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    cx.update(|cx| cx.set_global(SettingsDir(dir.path().to_owned())));
    save_both(cx, &t).await;
    t.dispatch(
        cx,
        "cluster_sets::DeleteClusterSet",
        json!({ "name": "both" }),
    )
    .unwrap();
    cx.wait_for(t.window, WAIT, |window, _| window.find_all("ok").len() == 1)
        .await;
    let shown = cx
        .update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            confirm::shown(window, cx)
        })
        .unwrap()
        .expect("the confirmation");
    assert_eq!(shown.title, "Delete both?");
    assert_eq!(saved_names(cx, &t), ["both"], "nothing is deleted yet");
    assert!(t.settings_toml().contains("both"));
    t.click(cx, "ok");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).sets.saved.is_empty()
    })
    .await;
    assert!(!t.settings_toml().contains("both"));
}

/// A listener that counts the connections made to it.
fn counting_listener() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    std::thread::spawn(move || {
        for _ in listener.incoming() {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    (url, connections)
}

#[gpui_kit::test]
async fn a_repointed_member_is_left_out_and_never_contacted(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (alpha, beta) = (LocalApi::start(&[]), LocalApi::start(&[]));
    let (moved_to, connections) = counting_listener();
    let urls = (alpha.url.clone(), beta.url.clone(), moved_to);
    let t = open_workspace_with(cx, dir.path(), move |dir, _| {
        let path = dir.join("kubeconfig");
        let contexts = [
            ("kind-alpha", &*urls.0),
            ("kind-beta", &*urls.1),
            ("kind-gamma", &*urls.2),
        ];
        std::fs::write(&path, kubeconfig_yaml(&contexts)).unwrap();
        vec![path]
    })
    .await;
    let pins = vec![
        ClusterPin::new("kind-alpha".to_owned(), alpha.url.clone()),
        ClusterPin::new("kind-beta".to_owned(), beta.url.clone()),
        ClusterPin::new("kind-gamma".to_owned(), "https://old.invalid".to_owned()),
    ];
    t.workspace.update(cx, |ws, _| {
        ws.sets.saved = vec![ClusterSet::new("three", pins).unwrap()];
    });
    t.dispatch(
        cx,
        "cluster_sets::OpenClusterSet",
        json!({ "name": "three" }),
    )
    .unwrap();
    t.wait_for_element(cx, "cluster-set-view").await;
    cx.run_until_parked();
    let (members, refused) = t.workspace.read_with(cx, |ws, cx| {
        let run = ws.sets.running.as_ref().unwrap();
        let members: Vec<String> = run.members.iter().map(|m| m.context().to_owned()).collect();
        (members, ws.session_member("kind-gamma", cx))
    });
    assert_eq!(members, ["kind-alpha", "kind-beta"]);
    assert!(
        matches!(refused, Ok(None)),
        "it is no member to act through"
    );
    let label = t.label(cx, "set-member-kind-gamma").unwrap();
    assert!(label.ends_with("Changed, left out"), "{label}");
    assert_eq!(connections.load(Ordering::SeqCst), 0, "never contacted");
}

#[gpui_kit::test]
async fn a_set_left_with_one_member_does_not_open(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    let alpha = t.workspace.read_with(cx, |ws, cx| {
        let summary = ws.cluster_summary("kind-alpha", cx).unwrap();
        summary.info.server.clone().unwrap()
    });
    let pins = vec![
        ClusterPin::new("kind-alpha".to_owned(), alpha),
        ClusterPin::new("kind-beta".to_owned(), "https://old.invalid".to_owned()),
    ];
    t.workspace.update(cx, |ws, _| {
        ws.sets.saved = vec![ClusterSet::new("pair", pins).unwrap()];
    });
    t.dispatch(
        cx,
        "cluster_sets::OpenClusterSet",
        json!({ "name": "pair" }),
    )
    .unwrap();
    wait_until_answered(cx, &t).await;
    let (open, error) = t
        .workspace
        .read_with(cx, |ws, _| (ws.sets.is_open(), ws.sets.error.clone()));
    assert!(!open);
    let error = error.expect("says why");
    assert!(
        error.contains("kind-beta") && error.contains("too few"),
        "{error}"
    );
    assert!(!t.server_was_contacted(), "nothing started");
}

/// Waits for the core's answer to the open on its way, which arrives from its own thread.
async fn wait_until_answered(cx: &mut TestAppContext, t: &TestWorkspace) {
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).sets.opening.is_none()
    })
    .await;
    cx.run_until_parked();
}

/// A saved set whose last member, `kind-gamma`, is defined in a file after one that can break.
struct SavedSet {
    t: TestWorkspace,
    _apis: (LocalApi, LocalApi),
    middle: std::path::PathBuf,
    last: std::path::PathBuf,
    gamma_url: String,
    gamma_connections: Arc<AtomicUsize>,
    settings: String,
}

const SPARE: (&str, &str) = ("kind-spare", "http://127.0.0.1:1");

/// Saves `members` as the set `saved`, over three kubeconfig files.
async fn saved_set(cx: &mut TestAppContext, dir: &std::path::Path, members: &[&str]) -> SavedSet {
    let (alpha, beta) = (LocalApi::start(&[]), LocalApi::start(&[]));
    let (gamma_url, gamma_connections) = counting_listener();
    let (first, middle, last) = (
        dir.join("kubeconfig-a"),
        dir.join("kubeconfig-b"),
        dir.join("kubeconfig-c"),
    );
    let files = vec![first.clone(), middle.clone(), last.clone()];
    let contexts = [("kind-alpha", &*alpha.url), ("kind-beta", &*beta.url)];
    std::fs::write(&first, kubeconfig_yaml(&contexts)).unwrap();
    std::fs::write(&middle, kubeconfig_yaml(&[SPARE])).unwrap();
    std::fs::write(&last, kubeconfig_yaml(&[("kind-gamma", &gamma_url)])).unwrap();
    let t = open_workspace_with(cx, dir, move |_, _| files).await;
    cx.update(|cx| cx.set_global(SettingsDir(dir.to_owned())));
    t.dispatch(cx, "cluster_sets::ToggleClusterPicking", Value::Null)
        .unwrap();
    for member in members {
        pick(cx, &t, member);
    }
    t.dispatch(
        cx,
        "cluster_sets::SaveClusterSet",
        json!({ "name": "saved" }),
    )
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        !workspace.read(cx).sets.saved.is_empty()
    })
    .await;
    let settings = t.settings_toml();
    SavedSet {
        t,
        _apis: (alpha, beta),
        middle,
        last,
        gamma_url,
        gamma_connections,
        settings,
    }
}

impl SavedSet {
    async fn reload(&self, cx: &mut TestAppContext) {
        let core = self.t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());
        core.reload_kubeconfig().await.ok();
        cx.run_until_parked();
    }

    fn open(&self, cx: &mut TestAppContext) {
        self.t
            .dispatch(
                cx,
                "cluster_sets::OpenClusterSet",
                json!({ "name": "saved" }),
            )
            .unwrap();
    }

    fn members(&self, cx: &mut TestAppContext) -> Vec<String> {
        self.t.workspace.read_with(cx, |ws, _| {
            let run = ws.sets.running.as_ref().unwrap();
            run.members.iter().map(|m| m.context().to_owned()).collect()
        })
    }

    fn break_middle_file(&self) {
        std::fs::write(&self.middle, "clusters: [").unwrap();
    }

    /// Opens the set and checks it runs without `kind-gamma`, shown as `says`, which it never
    /// contacted, and that the saved list did not change.
    async fn opens_without_gamma(&self, cx: &mut TestAppContext, says: &str) {
        self.open(cx);
        self.t.wait_for_element(cx, "cluster-set-view").await;
        cx.run_until_parked();
        assert_eq!(self.members(cx), ["kind-alpha", "kind-beta"]);
        let label = self.t.label(cx, "set-member-kind-gamma").unwrap();
        assert_eq!(label, format!("kind-gamma: {says}, left out"));
        let acts = self
            .t
            .workspace
            .read_with(cx, |ws, cx| ws.session_member("kind-gamma", cx));
        assert!(matches!(acts, Ok(None)), "it is no member to act through");
        assert_eq!(self.gamma_connections.load(Ordering::SeqCst), 0);
        assert_eq!(self.t.settings_toml(), self.settings, "the list is kept");
    }
}

const THREE: [&str; 3] = ["kind-alpha", "kind-beta", "kind-gamma"];

#[gpui_kit::test]
async fn a_removed_member_is_left_out(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let set = saved_set(cx, dir.path(), &THREE).await;
    std::fs::write(&set.last, kubeconfig_yaml(&[SPARE])).unwrap();
    set.reload(cx).await;
    set.opens_without_gamma(cx, "Removed").await;
}

#[gpui_kit::test]
async fn a_provisional_member_is_left_out(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let set = saved_set(cx, dir.path(), &THREE).await;
    set.break_middle_file();
    set.reload(cx).await;
    let (provisional, paused) = set.t.workspace.read_with(cx, |ws, cx| {
        let gamma = ws.cluster_summary("kind-gamma", cx).unwrap();
        (gamma.provisional, gamma.paused)
    });
    assert!(provisional && !paused, "unchanged behind a broken file");
    set.opens_without_gamma(cx, "Provisional").await;
}

#[gpui_kit::test]
async fn a_paused_member_is_left_out(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let set = saved_set(cx, dir.path(), &THREE).await;
    let (elsewhere, elsewhere_connections) = counting_listener();
    set.break_middle_file();
    std::fs::write(&set.last, kubeconfig_yaml(&[("kind-gamma", &elsewhere)])).unwrap();
    set.reload(cx).await;
    let paused = set.t.workspace.read_with(cx, |ws, cx| {
        ws.cluster_summary("kind-gamma", cx).unwrap().paused
    });
    assert!(paused, "its definition differs behind a broken file");
    set.opens_without_gamma(cx, "Paused").await;
    assert_eq!(elsewhere_connections.load(Ordering::SeqCst), 0);
}

impl SavedSet {
    fn shown_clusters(&self, cx: &mut TestAppContext) -> Vec<ClusterSummary> {
        self.t
            .workspace
            .read_with(cx, |ws, cx| ws.model.read(cx).clusters.clone())
    }

    /// Puts the workspace's view of the clusters back to `shown`, while the core has moved on.
    fn show_stale_clusters(&self, cx: &mut TestAppContext, shown: Vec<ClusterSummary>) {
        self.t.workspace.update(cx, |ws, cx| {
            ws.model.update(cx, |model, _| model.clusters = shown);
        });
    }
}

#[gpui_kit::test]
async fn a_member_turning_provisional_after_the_view_looked_is_left_out(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let set = saved_set(cx, dir.path(), &THREE).await;
    let before = set.shown_clusters(cx);
    set.break_middle_file();
    set.reload(cx).await;
    set.show_stale_clusters(cx, before);
    set.opens_without_gamma(cx, "Provisional").await;
}

#[gpui_kit::test]
async fn a_member_back_after_the_view_looked_joins(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let set = saved_set(cx, dir.path(), &THREE).await;
    std::fs::write(&set.last, kubeconfig_yaml(&[SPARE])).unwrap();
    set.reload(cx).await;
    let without_gamma = set.shown_clusters(cx);
    let back = kubeconfig_yaml(&[("kind-gamma", &set.gamma_url)]);
    std::fs::write(&set.last, back).unwrap();
    set.reload(cx).await;
    set.show_stale_clusters(cx, without_gamma);
    set.open(cx);
    set.t.wait_for_element(cx, "cluster-set-view").await;
    cx.run_until_parked();
    assert_eq!(set.members(cx), THREE);
}

#[gpui_kit::test]
async fn an_unsaved_set_is_titled_by_the_members_admitted(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let set = saved_set(cx, dir.path(), &THREE).await;
    set.t.workspace.update(cx, |ws, _| {
        ws.sets.picked = ws.sets.saved[0].members.clone();
    });
    std::fs::write(&set.last, kubeconfig_yaml(&[SPARE])).unwrap();
    set.reload(cx).await;
    set.t
        .dispatch(cx, "cluster_sets::OpenPickedClusters", Value::Null)
        .unwrap();
    set.t.wait_for_element(cx, "cluster-set-view").await;
    let label = set
        .t
        .workspace
        .read_with(cx, |ws, _| ws.sets.running.as_ref().unwrap().label.clone());
    assert_eq!(label, "2 clusters");
}

#[gpui_kit::test]
async fn a_refused_open_stops_the_open_it_replaced(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let set = saved_set(cx, dir.path(), &["kind-alpha", "kind-beta"]).await;
    let window = set.t.window;
    let workspace = set.t.workspace.clone();
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            let saved = ws.sets.saved[0].clone();
            let mut stale = saved.members.clone();
            stale[1] = ClusterPin::new("kind-beta".to_owned(), "https://old.invalid".to_owned());
            ws.open_set(saved.members, None, window, cx);
            ws.open_set(stale, None, window, cx);
        })
    })
    .unwrap();
    let core = set.t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        cx.run_until_parked();
        let settled = set
            .t
            .workspace
            .read_with(cx, |ws, _| ws.sets.opening.is_none());
        if settled && core.running_set().await.is_empty() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the core kept a set");
        cx.background_executor
            .timer(std::time::Duration::from_millis(10))
            .await;
    }
    let (open, error) = set
        .t
        .workspace
        .read_with(cx, |ws, _| (ws.sets.is_open(), ws.sets.error.clone()));
    assert!(!open, "no view is left over a set nobody asked for");
    assert!(error.expect("says why").contains("too few"));
}

#[gpui_kit::test]
async fn too_few_current_members_refuse_the_set_and_name_why(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let set = saved_set(cx, dir.path(), &["kind-alpha", "kind-gamma"]).await;
    set.break_middle_file();
    set.reload(cx).await;
    set.open(cx);
    wait_until_answered(cx, &set.t).await;
    let (open, error) = set
        .t
        .workspace
        .read_with(cx, |ws, _| (ws.sets.is_open(), ws.sets.error.clone()));
    assert!(!open);
    let error = error.expect("says why");
    assert!(
        error.contains("kind-gamma (provisional)") && error.contains("too few"),
        "{error}"
    );
    assert_eq!(set.gamma_connections.load(Ordering::SeqCst), 0);
    assert_eq!(set.t.settings_toml(), set.settings);
}

#[gpui_kit::test]
async fn a_member_that_returns_is_back_the_next_time_the_set_opens(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let set = saved_set(cx, dir.path(), &THREE).await;
    std::fs::write(&set.last, kubeconfig_yaml(&[SPARE])).unwrap();
    set.reload(cx).await;
    set.opens_without_gamma(cx, "Removed").await;
    set.t
        .dispatch(cx, "cluster_sets::CloseClusterSet", Value::Null)
        .unwrap();
    cx.run_until_parked();

    let back = kubeconfig_yaml(&[("kind-gamma", &set.gamma_url)]);
    std::fs::write(&set.last, back).unwrap();
    set.reload(cx).await;
    set.open(cx);
    set.t.wait_for_element(cx, "cluster-set-view").await;
    cx.run_until_parked();
    assert_eq!(set.members(cx), THREE);
    assert!(
        set.t.label(cx, "set-member-kind-gamma").is_none(),
        "no longer left out"
    );
    assert_eq!(set.t.settings_toml(), set.settings);
}

#[gpui_kit::test]
async fn a_cmd_click_chooses_and_a_plain_click_does_not(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    cx.update(|cx| {
        let cmd = gpui_kit::Modifiers::secondary_key(cx);
        assert!(click_chooses(false, cmd, cx));
        assert!(click_chooses(true, gpui_kit::Modifiers::default(), cx));
        assert!(!click_chooses(false, gpui_kit::Modifiers::default(), cx));
    });
    pick(cx, &t, "kind-beta");
    let (picking, picked) = t
        .workspace
        .read_with(cx, |ws, _| (ws.sets.picking(), ws.sets.picked.clone()));
    assert!(picking, "the first choice starts choosing");
    assert_eq!(picked.len(), 1);
    assert_eq!(picked[0].context, "kind-beta");
    assert!(!t.server_was_contacted(), "choosing connects to nothing");
}

#[test]
fn every_set_and_combined_action_is_bound() {
    use crate::ui::list::combined::CombinedResourceList;
    use crate::ui::registry::{Scope, tests::assert_binds_scope};
    assert_binds_scope(Scope::ClusterSets, Workspace::bind_set_actions);
    assert_binds_scope(Scope::CombinedList, CombinedResourceList::bind_actions);
}

/// Asks for a fleet restart of the open row and waits for its one confirmation.
async fn offer_fleet_restart(cx: &mut TestAppContext, t: &TestWorkspace) -> confirm::DialogText {
    let (workspace, window) = (t.workspace.clone(), t.window);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            assert_eq!(
                fleet_view::available(ws, FleetChoice::RolloutRestart, cx),
                Ok(())
            );
            fleet_view::request(ws, FleetChoice::RolloutRestart, window, cx);
        })
    })
    .unwrap();
    cx.wait_for(t.window, WAIT, |window, _| window.find_all("ok").len() == 1)
        .await;
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(t.window, |_, window, cx| confirm::shown(window, cx))
        .unwrap()
        .expect("the plan")
}

// covers UP-100: one confirmation names both members after a dry run on each, and the run
// restarts each once, reporting each.
#[gpui_kit::test]
async fn a_fleet_restart_runs_on_each_member_after_one_confirmation(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let row = open_beta_row(cx, dir.path()).await;
    let t = &row.t;
    let plan = offer_fleet_restart(cx, t).await;
    assert_eq!(plan.title, "Restart web on 2 of 2 clusters?");
    assert!(
        plan.text.contains("Runs on: kind-alpha, kind-beta"),
        "{}",
        plan.text
    );
    assert_eq!(row.alpha_writes.load(Ordering::SeqCst), 1, "its dry run");
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 1, "its dry run");
    t.click(cx, "ok");
    t.wait_for_label(cx, "fleet-summary", "Finished: 2 done.")
        .await;
    assert_eq!(row.alpha_writes.load(Ordering::SeqCst), 2);
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 2);
    assert_eq!(
        t.label(cx, ("fleet-result", 0usize)).as_deref(),
        Some("kind-alpha: done")
    );
}

// covers UP-100: the first member's failure stops the run: the next gets its dry run and nothing
// else, and the results say which failed and which was not tried.
#[gpui_kit::test]
async fn a_fleet_failure_stops_the_run_and_says_so(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // Alpha's dry run passes; its restart is refused.
    let row = open_beta_row_answering(cx, dir.path(), &[200, 422]).await;
    let t = &row.t;
    offer_fleet_restart(cx, t).await;
    t.click(cx, "ok");
    t.wait_for_label(cx, "fleet-summary", "Finished: 1 failed, 1 not tried.")
        .await;
    assert_eq!(
        row.alpha_writes.load(Ordering::SeqCst),
        2,
        "sent once, not retried"
    );
    assert_eq!(
        row.beta_writes.load(Ordering::SeqCst),
        1,
        "its dry run only"
    );
    assert_eq!(
        t.label(cx, ("fleet-result", 1usize)).as_deref(),
        Some("kind-beta: not tried: an earlier cluster failed")
    );
}

/// Asks for `choice` on the open row across the set, without waiting.
fn ask_fleet(cx: &mut TestAppContext, t: &TestWorkspace, choice: FleetChoice) {
    let (workspace, window) = (t.workspace.clone(), t.window);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            assert_eq!(fleet_view::available(ws, choice, cx), Ok(()));
            fleet_view::request(ws, choice, window, cx);
        })
    })
    .unwrap();
}

/// Waits until the newest confirmation's title is `title`, and returns it.
async fn wait_for_dialog(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    title: &'static str,
) -> confirm::DialogText {
    let shown = move |cx: &mut TestAppContext| {
        cx.update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            confirm::shown(window, cx)
        })
        .unwrap()
    };
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if let Some(dialog) = shown(cx).filter(|d| d.title == title) {
            return dialog;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no dialog \"{title}\": {:?}",
            shown(cx).map(|d| d.title)
        );
        cx.background_executor
            .timer(std::time::Duration::from_millis(10))
            .await;
        cx.run_until_parked();
    }
}

/// Waits until a single dialog is open, so its OK is the only one, and presses it.
async fn press_only_ok(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.wait_for(t.window, WAIT, |window, _| window.find_all("ok").len() == 1)
        .await;
    t.click(cx, "ok");
}

// covers UP-101: a delete across the set lists each member with its dry run; one whose dry run
// failed cannot be chosen and is sent nothing more, and the rest run after one confirmation.
#[gpui_kit::test]
async fn a_fleet_delete_leaves_a_failed_dry_run_out(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // Alpha refuses its delete's dry run.
    let row = open_beta_row_answering(cx, dir.path(), &[422]).await;
    let t = &row.t;
    ask_fleet(cx, t, FleetChoice::Delete);
    wait_for_dialog(cx, t, "Delete web: choose the clusters").await;
    let alpha = t.label(cx, ("fleet-target-says", 0usize)).unwrap();
    assert!(alpha.starts_with("left out: "), "{alpha}");
    assert_eq!(
        t.label(cx, ("fleet-target-says", 1usize)).as_deref(),
        Some("Deletes it. This cannot be undone.")
    );
    assert_eq!(
        t.find(cx, ("fleet-target", 0usize)).unwrap().checked(),
        Some(false)
    );
    assert_eq!(
        t.find(cx, ("fleet-target", 1usize)).unwrap().checked(),
        Some(true)
    );
    assert_eq!(row.alpha_writes.load(Ordering::SeqCst), 1, "its dry run");
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 1, "its dry run");
    // A member whose dry run failed cannot be put back.
    t.click(cx, ("fleet-target", 0usize));
    cx.run_until_parked();
    assert_eq!(
        t.find(cx, ("fleet-target", 0usize)).unwrap().checked(),
        Some(false)
    );
    t.click(cx, "ok");
    let plan = wait_for_dialog(cx, t, "Delete web on 1 of 2 clusters?").await;
    assert!(plan.text.contains("Runs on: kind-beta"), "{}", plan.text);
    assert_eq!(plan.ok, "Delete on 1 Cluster");
    assert!(plan.default_cancel, "Enter cancels a delete");
    press_only_ok(cx, t).await;
    t.wait_for_label(cx, "fleet-summary", "Finished: 1 done, 1 left out.")
        .await;
    assert_eq!(
        row.alpha_writes.load(Ordering::SeqCst),
        1,
        "left out: nothing more"
    );
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 2);
}

// covers UP-101, ADR-0015: a member whose dry run passed can still be left out of a delete; the
// plan names only the members chosen, and the one left out gets nothing more.
#[gpui_kit::test]
async fn a_healthy_member_can_be_left_out_of_a_fleet_delete(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let row = open_beta_row(cx, dir.path()).await;
    let t = &row.t;
    ask_fleet(cx, t, FleetChoice::Delete);
    wait_for_dialog(cx, t, "Delete web: choose the clusters").await;
    t.click(cx, ("fleet-target", 0usize));
    cx.run_until_parked();
    assert_eq!(
        t.find(cx, ("fleet-target", 0usize)).unwrap().checked(),
        Some(false)
    );
    // With none chosen there is nothing to go on to.
    t.click(cx, ("fleet-target", 1usize));
    cx.run_until_parked();
    t.click(cx, "ok");
    cx.run_until_parked();
    let shown = cx
        .update_window(t.window, |_, window, cx| confirm::shown(window, cx))
        .unwrap()
        .map(|d| d.title);
    assert_eq!(shown.as_deref(), Some("Delete web: choose the clusters"));
    t.click(cx, ("fleet-target", 1usize));
    cx.run_until_parked();
    t.click(cx, "ok");
    let plan = wait_for_dialog(cx, t, "Delete web on 1 of 2 clusters?").await;
    assert!(plan.text.contains("Runs on: kind-beta"), "{}", plan.text);
    assert!(
        plan.text.contains("kind-alpha: left out: you left it out"),
        "{}",
        plan.text
    );
    press_only_ok(cx, t).await;
    t.wait_for_label(cx, "fleet-summary", "Finished: 1 done, 1 left out.")
        .await;
    assert_eq!(
        row.alpha_writes.load(Ordering::SeqCst),
        1,
        "its dry run only"
    );
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 2);
}

// covers UP-101: an apply across the set opens the selected object's manifest, dry-runs it on
// each member, shows each member's change, and after one confirmation applies it on each.
#[gpui_kit::test]
async fn a_fleet_apply_reviews_the_manifest_on_each_member_then_applies_it(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let row = open_beta_row(cx, dir.path()).await;
    let t = &row.t;
    ask_fleet(cx, t, FleetChoice::ApplyYaml);
    wait_for_dialog(cx, t, "Apply YAML to web across the set").await;
    assert_eq!(row.alpha_writes.load(Ordering::SeqCst), 0);
    t.click(cx, "ok");
    wait_for_dialog(cx, t, "Apply YAML to web: choose the clusters").await;
    press_only_ok(cx, t).await;
    let plan = wait_for_dialog(cx, t, "Apply YAML to web on 2 of 2 clusters?").await;
    assert!(plan.text.contains("kind-alpha: "), "{}", plan.text);
    assert_eq!(plan.ok, "Apply on 2 Clusters");
    assert_eq!(row.alpha_writes.load(Ordering::SeqCst), 1, "its dry run");
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 1, "its dry run");
    press_only_ok(cx, t).await;
    t.wait_for_label(cx, "fleet-summary", "Finished: 2 done.")
        .await;
    assert_eq!(row.alpha_writes.load(Ordering::SeqCst), 2);
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 2);
}

// covers UP-101: from an apply's target list the manifest opens again as typed, to edit and
// review again; nothing is written meanwhile.
#[gpui_kit::test]
async fn a_fleet_apply_goes_back_to_its_manifest_as_typed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let row = open_beta_row(cx, dir.path()).await;
    let t = &row.t;
    ask_fleet(cx, t, FleetChoice::ApplyYaml);
    wait_for_dialog(cx, t, "Apply YAML to web across the set").await;
    t.type_text(cx, "x-draft: kept\n");
    t.click(cx, "ok");
    wait_for_dialog(cx, t, "Apply YAML to web: choose the clusters").await;
    t.click(cx, "fleet-edit-again");
    wait_for_dialog(cx, t, "Apply YAML to web across the set").await;
    press_only_ok(cx, t).await;
    wait_for_dialog(cx, t, "Apply YAML to web: choose the clusters").await;
    let puts = row._apis.0.puts();
    assert_eq!(puts.len(), 2, "two reviews, each a dry run");
    assert!(puts.iter().all(|(dry_run, _)| *dry_run));
    assert!(
        puts.iter().all(|(_, sent)| sent["x-draft"] == "kept"),
        "the draft went to both reviews: {puts:?}"
    );
}

// covers UP-100, UP-54: protected members are left out before any request; with every member
// protected there is nothing to confirm.
#[gpui_kit::test]
async fn a_fleet_action_leaves_protected_members_out(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.toml"),
        "protect_local_clusters = true\n",
    )
    .unwrap();
    let row = open_beta_row(cx, dir.path()).await;
    let t = &row.t;
    let plan = offer_fleet_restart(cx, t).await;
    assert_eq!(plan.title, "Nothing to run for web");
    assert!(plan.cancel.is_empty());
    assert!(plan.text.contains("kind-beta: left out: "), "{}", plan.text);
    t.click(cx, "ok");
    cx.run_until_parked();
    assert_eq!(row.alpha_writes.load(Ordering::SeqCst), 0);
    assert_eq!(row.beta_writes.load(Ordering::SeqCst), 0);
}

// A set's row whose member the set reports paused offers no Shell: the row menu says why, and
// the action's guard agrees. It comes back once the member runs again.
#[gpui_kit::test]
async fn a_paused_set_member_shows_shell_disabled_with_the_reason(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    connect(cx, &t, "kind-alpha");
    connect(cx, &t, "kind-beta");
    open_both(cx, &t).await;
    select_row(cx, &t, "kind-beta");
    let set_paused = |paused, cx: &mut TestAppContext| {
        t.workspace.update(cx, |ws, cx| {
            // Set directly: the row stays selected, as it does while an edit is unapplied.
            ws.sets.running.as_mut().unwrap().reports = vec![MemberReport {
                cluster: "kind-beta".into(),
                status: MemberStatus::Live,
                paused,
            }];
            cx.notify();
        });
    };
    let shell = |cx: &mut TestAppContext| {
        t.workspace.read_with(cx, |ws, cx| {
            let menu = crate::ui::row_menu::row_actions(ws, cx).unwrap();
            let guard = ws.sessions_blocked("kind-beta", cx);
            (menu.shell, guard)
        })
    };
    set_paused(true, cx);
    let (menu, guard) = shell(cx);
    let why = menu.unwrap_err();
    assert!(why.contains("paused"), "{why}");
    assert_eq!(guard.as_deref(), Some(why.as_str()));

    set_paused(false, cx);
    // Any check of the open row while paused closes it, as it should: choose it again.
    select_row(cx, &t, "kind-beta");
    let (menu, guard) = shell(cx);
    // The pod itself may not be fetched in this test; the pause is no longer the reason.
    assert!(!menu.is_err_and(|why| why.contains("paused")));
    assert_eq!(guard, None);
}

/// Waits for the unsaved-set question and checks what it offers.
async fn asked_about_unsaved_set(cx: &mut TestAppContext, t: &TestWorkspace) {
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |window, cx| {
        workspace
            .update(cx, |ws, cx| ws.view_state(window, cx).dialog)
            .is_some_and(|dialog| dialog.title == "Leave this unsaved set?")
    })
    .await;
}

fn set_is_open(cx: &mut TestAppContext, t: &TestWorkspace) -> bool {
    t.workspace.read_with(cx, |ws, _| ws.sets.is_open())
}

// #766: closing a set opened together asks first; Keep open keeps it, Discard closes it.
#[gpui_kit::test]
async fn leaving_an_unsaved_set_asks(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    open_both(cx, &t).await;
    t.dispatch(cx, "cluster_sets::CloseClusterSet", Value::Null)
        .unwrap();
    asked_about_unsaved_set(cx, &t).await;
    t.press(cx, "escape");
    answered(cx, &t).await;
    assert!(set_is_open(cx, &t), "Keep open keeps the set");
    t.dispatch(cx, "cluster_sets::CloseClusterSet", Value::Null)
        .unwrap();
    asked_about_unsaved_set(cx, &t).await;
    t.click(cx, "ok");
    answered(cx, &t).await;
    assert!(!set_is_open(cx, &t), "Discard closes it");
}

// #762, #766: a set opened together is saved from its own bar, after which leaving asks nothing.
#[gpui_kit::test]
async fn an_open_set_is_saved_from_its_bar(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    cx.update(|cx| cx.set_global(SettingsDir(dir.path().to_owned())));
    open_both(cx, &t).await;
    assert!(t.find(cx, "save-open-set").is_some(), "the bar offers Save");
    t.dispatch(
        cx,
        "cluster_sets::SaveClusterSet",
        json!({ "name": "pair" }),
    )
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        !workspace.read(cx).sets.is_unsaved() && !workspace.read(cx).sets.saved.is_empty()
    })
    .await;
    assert_eq!(saved_names(cx, &t), ["pair"]);
    assert!(t.settings_toml().contains("pair"));
    assert!(t.find(cx, "save-open-set").is_none(), "saved: no Save left");
    t.dispatch(cx, "cluster_sets::CloseClusterSet", Value::Null)
        .unwrap();
    assert!(!set_is_open(cx, &t), "a saved set closes without asking");
}

// #767: saving chosen clusters leaves choosing mode.
#[gpui_kit::test]
async fn saving_chosen_clusters_leaves_choosing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    cx.update(|cx| cx.set_global(SettingsDir(dir.path().to_owned())));
    save_both(cx, &t).await;
    t.workspace.read_with(cx, |ws, _| {
        assert!(!ws.sets.picking(), "choosing is off after the save");
        assert!(ws.sets.picked.is_empty());
    });
}

// #759: with a set open there is no namespace filter to change, so its actions are off.
#[gpui_kit::test]
async fn an_open_set_has_no_namespace_filter_actions(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    open_both(cx, &t).await;
    for (action, args) in [
        ("workspace::SetNamespaces", json!({ "namespaces": ["a"] })),
        ("workspace::ToggleNamespace", json!({ "namespace": "a" })),
        ("workspace::AddNamespace", json!({ "namespace": "a" })),
    ] {
        assert!(
            t.dispatch(cx, action, args).is_err(),
            "{action} is refused in a set"
        );
    }
}

// #760: clicking a cluster outside the set leaves it, also when that cluster is already active,
// and the main area shows that cluster again instead of an empty page.
#[gpui_kit::test]
async fn clicking_the_active_non_member_leaves_the_set(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, servers| {
        let path = dir.join("kubeconfig");
        let contexts = [
            ("kind-alpha", servers.silent.as_str()),
            ("kind-beta", servers.silent.as_str()),
            ("kind-gamma", servers.silent.as_str()),
        ];
        std::fs::write(&path, kubeconfig_yaml(&contexts)).unwrap();
        vec![path]
    })
    .await;
    t.dispatch(
        cx,
        "workspace::SelectCluster",
        json!({ "cluster": "kind-gamma" }),
    )
    .unwrap();
    open_both(cx, &t).await;
    assert!(set_is_open(cx, &t));
    t.dispatch(
        cx,
        "workspace::SelectCluster",
        json!({ "cluster": "kind-gamma" }),
    )
    .unwrap();
    asked_about_unsaved_set(cx, &t).await;
    t.click(cx, "ok");
    answered(cx, &t).await;
    assert!(!set_is_open(cx, &t), "the set closed");
    assert!(t.find(cx, "cluster-set-view").is_none());
}

// #767: Return in the set name field saves, as the Save Set button does.
#[gpui_kit::test]
async fn return_in_the_name_field_saves(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_quiet_workspace(cx, dir.path()).await;
    cx.update(|cx| cx.set_global(SettingsDir(dir.path().to_owned())));
    t.dispatch(cx, "cluster_sets::ToggleClusterPicking", Value::Null)
        .unwrap();
    pick(cx, &t, "kind-alpha");
    pick(cx, &t, "kind-beta");
    let input = t
        .workspace
        .read_with(cx, |ws, _| ws.sets.name_input.clone());
    cx.update_window(t.window, |_, window, cx| {
        input.update(cx, |input, cx| input.set_value("pair", window, cx));
        input.update(cx, |_, cx| {
            cx.emit(gpui_kit::component::input::InputEvent::PressEnter {
                secondary: false,
                shift: false,
            })
        });
    })
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        !workspace.read(cx).sets.saved.is_empty()
    })
    .await;
    assert_eq!(saved_names(cx, &t), ["pair"]);
}

/// What the set's state says is wrong with `cluster`, as the automation channel reads it.
fn member_note(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str) -> Option<String> {
    t.workspace.read_with(cx, |ws, cx| {
        let open = ws.sets_view_state(cx).open.expect("an open set");
        open.members
            .into_iter()
            .find(|m| m.cluster == cluster)
            .expect("a member")
            .note
    })
}

fn report(cluster: &str, status: MemberStatus, paused: bool) -> MemberReport {
    MemberReport {
        cluster: cluster.into(),
        status,
        paused,
    }
}

fn signed_out(cluster: &str) -> MemberReport {
    let reason = SafeErrorText::reduced("expired");
    report(cluster, MemberStatus::SignInNeeded { reason }, false)
}

fn failed(cluster: &str) -> MemberReport {
    let error = SafeErrorText::reduced("refused");
    report(cluster, MemberStatus::Failed { error }, false)
}

/// The empty-state block's rows as `(cluster, cause line, action label)`.
fn empty_block(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
) -> Option<Vec<(String, &'static str, Option<&'static str>)>> {
    t.workspace.read_with(cx, |ws, cx| {
        let run = ws.sets.running.as_ref()?;
        Some(
            ws.set_empty_state(run, cx)?
                .into_iter()
                .map(|l| {
                    (
                        l.cluster,
                        l.cause.line(),
                        l.cause.action().map(|a| a.label()),
                    )
                })
                .collect(),
        )
    })
}

async fn open_with_reports(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    reports: Vec<MemberReport>,
) -> TestWorkspace {
    let t = open_quiet_workspace(cx, dir).await;
    connect(cx, &t, "kind-alpha");
    connect(cx, &t, "kind-beta");
    open_both(cx, &t).await;
    t.workspace
        .update(cx, |ws, cx| ws.set_member_reports(reports, cx));
    cx.run_until_parked();
    t
}

#[gpui_kit::test]
async fn no_member_signed_in_shows_one_row_each_instead_of_the_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_reports(
        cx,
        dir.path(),
        vec![signed_out("kind-alpha"), signed_out("kind-beta")],
    )
    .await;
    assert_eq!(
        empty_block(cx, &t).unwrap(),
        [
            ("kind-alpha".into(), "Sign-in needed", Some("Sign in")),
            ("kind-beta".into(), "Sign-in needed", Some("Sign in")),
        ]
    );
    t.wait_for_element(cx, "set-members-empty").await;
    assert_eq!(
        t.label(cx, "set-member-empty-kind-alpha").as_deref(),
        Some("kind-alpha: Sign-in needed")
    );
    assert!(t.label(cx, "set-member-action-kind-beta").is_some());
    // The chips, the empty table and the footer are gone with the list.
    assert!(t.find(cx, "combined-members").is_none());
    assert!(t.find(cx, "combined-kind-note").is_none());
    // No consent toggle, no command, no per-member header chip.
    assert!(t.find(cx, "background-sign-in-row-kind-alpha").is_none());
    assert!(t.find(cx, "set-member-kind-alpha").is_none());
}

// A member's Sign in asks the core to sign that member in once, bound to the member as the set
// admitted it; it selects nothing. Here the core's own member is not waiting for a sign-in, so
// it refuses, and the set says why.
#[gpui_kit::test]
async fn sign_in_asks_the_core_for_that_member_and_selects_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_reports(
        cx,
        dir.path(),
        vec![signed_out("kind-alpha"), signed_out("kind-beta")],
    )
    .await;
    let active = t.workspace.read_with(cx, |ws, cx| {
        ws.model
            .read(cx)
            .active_cluster()
            .map(|c| c.info.name.clone())
    });
    t.wait_for_element(cx, "set-member-action-kind-beta").await;

    t.click(cx, "set-member-action-kind-beta");
    cx.run_until_parked();

    let (error, now) = t.workspace.read_with(cx, |ws, cx| {
        (
            ws.sets.error.clone(),
            ws.model
                .read(cx)
                .active_cluster()
                .map(|c| c.info.name.clone()),
        )
    });
    let error = error.expect("the core answered");
    assert!(error.contains("kind-beta"), "{error}");
    assert_eq!(now, active, "nothing was selected");
}

#[gpui_kit::test]
async fn paused_members_are_listed_with_why_and_no_action(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let paused = |c: &str| report(c, MemberStatus::Live, true);
    let t = open_with_reports(
        cx,
        dir.path(),
        vec![paused("kind-alpha"), paused("kind-beta")],
    )
    .await;
    let rows = empty_block(cx, &t).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|(_, line, action)| line.starts_with("Paused") && action.is_none()),
        "{rows:?}"
    );
}

#[gpui_kit::test]
async fn failed_members_are_listed_with_retry(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_reports(
        cx,
        dir.path(),
        vec![failed("kind-alpha"), failed("kind-beta")],
    )
    .await;
    assert_eq!(
        empty_block(cx, &t).unwrap(),
        [
            ("kind-alpha".into(), "Connection failed", Some("Retry")),
            ("kind-beta".into(), "Connection failed", Some("Retry")),
        ]
    );
}

#[gpui_kit::test]
async fn mixed_causes_each_get_their_own_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_reports(
        cx,
        dir.path(),
        vec![
            signed_out("kind-alpha"),
            report("kind-beta", MemberStatus::Live, true),
        ],
    )
    .await;
    let rows = empty_block(cx, &t).unwrap();
    assert_eq!(rows[0].2, Some("Sign in"));
    assert_eq!(rows[1].2, None);
    assert!(rows[1].1.starts_with("Paused"), "{rows:?}");
}

#[gpui_kit::test]
async fn a_signed_out_member_beside_a_working_one_is_only_a_chip(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_reports(cx, dir.path(), vec![signed_out("kind-beta")]).await;
    assert_eq!(empty_block(cx, &t), None, "kind-alpha can still show rows");
    let (signed_out, note) = t.workspace.read_with(cx, |ws, cx| {
        let run = ws.sets.running.as_ref().unwrap();
        (
            ws.signed_out_members(run, cx),
            run.list.read(cx).kind_note().map(str::to_owned),
        )
    });
    assert_eq!(
        signed_out,
        std::collections::HashSet::from(["kind-beta".to_owned()])
    );
    // No "not confirmed" footer for the sign-in cause.
    assert!(
        note.as_deref().is_none_or(|n| !n.contains("kind-beta")),
        "{note:?}"
    );
    assert!(t.find(cx, "set-members-empty").is_none());
    assert!(t.find(cx, "background-sign-in-row-kind-beta").is_none());
    assert!(
        t.find(cx, "set-member-kind-beta").is_none(),
        "no header chip"
    );
}
