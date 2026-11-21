//! View tests for the write confirmation's flow, over a real review and a small local API server:
//! what Cancel and Confirm send, and that a write that started always reports back.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use flint_core::protection::WriteRefusal;
use flint_core::write::{WriteError, WriteOutcome, WriteProposal, WriteTarget};
use flint_core::{ConnectionState, KindId};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Gvr, Options, WriteRecord};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::resource_actions_tests::{LocalApi, connected};
use crate::ui::registry;
use crate::ui::support_tests::{TestWorkspace, WAIT, kubeconfig_yaml, open_workspace_with};
use crate::ui::write_confirm;

/// Long enough for a second write, had one been sent, to reach the local server.
const SETTLE: Duration = Duration::from_millis(150);

/// What one open confirmation has done so far.
#[derive(Default)]
struct Flow {
    /// Times `begin` was asked.
    begun: Cell<usize>,
    /// Every result `on_done` was called with.
    done: RefCell<Vec<Result<WriteOutcome, WriteError>>>,
}

impl Flow {
    fn done_count(&self) -> usize {
        self.done.borrow().len()
    }

    /// The one result reported.
    fn result(&self) -> Result<WriteOutcome, WriteError> {
        let done = self.done.borrow();
        assert_eq!(done.len(), 1, "on_done runs exactly once: {done:?}");
        done[0].clone()
    }
}

fn web(api: &LocalApi) -> WriteTarget {
    WriteTarget {
        cluster: "alpha".into(),
        server: Some(api.url.clone()),
        kind: KindId::new("apps", "deployments"),
        namespace: Some("demo".to_owned()),
        name: "web".to_owned(),
        uid: Some("uid-web".to_owned()),
        resource_version: Some("7".to_owned()),
    }
}

fn draw(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.run_until_parked();
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// Opens the confirmation of a real review of `proposal` on `web`. `begin` is what the caller's
/// claim answers.
async fn open(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    api: &LocalApi,
    proposal: WriteProposal,
    begin: bool,
) -> Rc<Flow> {
    open_on(cx, t, web(api), proposal, begin).await
}

/// Opens the confirmation of a real review of `proposal` on `target`.
async fn open_on(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    target: WriteTarget,
    proposal: WriteProposal,
    begin: bool,
) -> Rc<Flow> {
    let (core, names) = t
        .workspace
        .read_with(cx, |ws, cx| (ws.core(cx).clone(), ws.cluster_names(cx)));
    let review = core
        .review_write(target, proposal)
        .await
        .expect("the review");
    let flow = Rc::new(Flow::default());
    let (asked, finished) = (flow.clone(), flow.clone());
    cx.update_window(t.window, |_, window, cx| {
        write_confirm::open_noted(
            window,
            cx,
            core,
            review,
            &names,
            None,
            move |_| {
                asked.begun.set(asked.begun.get() + 1);
                begin
            },
            move |result, _| finished.done.borrow_mut().push(result),
        );
    })
    .unwrap();
    t.wait_for_element(cx, "write-confirm-description").await;
    draw(cx, t);
    flow
}

/// Waits for `on_done`, without a window: the one it opened in may have closed.
fn wait_done(cx: &mut TestAppContext, flow: &Flow) {
    let start = Instant::now();
    while flow.done_count() == 0 {
        assert!(start.elapsed() < WAIT, "on_done never ran");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(2));
    }
    // A second write or a second report would land now.
    std::thread::sleep(SETTLE);
    cx.run_until_parked();
}

fn wait_patches(api: &LocalApi, wanted: usize) {
    let start = Instant::now();
    while api.patches() < wanted {
        assert!(start.elapsed() < WAIT, "{wanted} writes never arrived");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn assert_nothing_ran(cx: &mut TestAppContext, t: &TestWorkspace, api: &LocalApi, flow: &Flow) {
    std::thread::sleep(SETTLE);
    draw(cx, t);
    assert!(!t.has_dialog(cx), "the dialog closed");
    assert_eq!(flow.begun.get(), 0, "begin is not asked");
    assert_eq!(flow.done_count(), 0, "on_done is not called");
    assert_eq!(api.patches(), 0, "no write");
}

const SCALE: WriteProposal = WriteProposal::Scale { to: 5 };

/// The workspace connected to `api` on a protected cluster that is unlocked for now.
async fn connected_unlocked(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    api: &LocalApi,
) -> TestWorkspace {
    std::fs::write(dir.join("settings.toml"), "protect_local_clusters = true\n").unwrap();
    let url = api.url.clone();
    let t = open_workspace_with(cx, dir, move |dir, _| {
        let path = dir.join("kubeconfig");
        std::fs::write(&path, kubeconfig_yaml(&[("alpha", &url)])).unwrap();
        vec![path]
    })
    .await;
    let core = t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());
    core.unlock("alpha", &api.url);
    t.activate(cx, "alpha").await;
    t.wait_for_state(cx, "alpha", |s| matches!(s, ConnectionState::Connected(_)))
        .await;
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .discovered("alpha")
            .is_some_and(|d| d.discovery.is_some())
    })
    .await;
    t
}

const ACT_PATH: &str = "/apis/apps/v1/namespaces/demo/deployments/act";

fn deployments() -> Gvr {
    Gvr::new("apps", "v1", "deployments")
}

/// The fake API server with a Deployment `act` of 2 replicas.
fn fake_api() -> FakeApiServer {
    let server = FakeApiServer::start_blocking(Options::default()).unwrap();
    server.set_object(
        &deployments(),
        json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": "act", "namespace": "demo", "uid": "uid-act",
                         "resourceVersion": "5000"},
            "spec": {"replicas": 2, "template": {"metadata": {}}},
        }),
    );
    server
}

/// `act` as it is live: the server sets its own `resourceVersion`.
fn act(server: &FakeApiServer) -> WriteTarget {
    let live = server
        .object(&deployments(), Some("demo"), "act")
        .expect("the workload");
    let version = live["metadata"]["resourceVersion"].as_str().unwrap();
    WriteTarget {
        cluster: CONTEXT.into(),
        server: Some(server.url()),
        kind: KindId::new("apps", "deployments"),
        namespace: Some("demo".to_owned()),
        name: "act".to_owned(),
        uid: Some("uid-act".to_owned()),
        resource_version: Some(version.to_owned()),
    }
}

/// The workspace connected to the fake API server.
async fn connected_to_fake(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    server: &FakeApiServer,
) -> TestWorkspace {
    let t = open_workspace_with(cx, dir, |dir, _| {
        let path = dir.join("kubeconfig");
        server.write_kubeconfig(&path, Some("demo")).unwrap();
        vec![path]
    })
    .await;
    t.activate(cx, CONTEXT).await;
    t.wait_for_state(cx, CONTEXT, |s| matches!(s, ConnectionState::Connected(_)))
        .await;
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model
            .read(cx)
            .discovered(CONTEXT)
            .is_some_and(|d| d.discovery.is_some())
    })
    .await;
    t
}

/// The one write the server received.
fn only_write(server: &FakeApiServer) -> WriteRecord {
    let writes = server.writes();
    assert_eq!(writes.len(), 1, "exactly one write: {writes:?}");
    writes[0].clone()
}

/// A confirmation of a real review, not in a dialog, so a test can run it again after its first
/// attempt ended, as a handler left in an old frame could.
async fn handler(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    api: &LocalApi,
    begin: bool,
) -> (Rc<Flow>, impl Fn(&mut gpui_kit::App) + 'static) {
    let core = t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());
    let review = core
        .review_write(web(api), SCALE)
        .await
        .expect("the review");
    let flow = Rc::new(Flow::default());
    let (asked, finished) = (flow.clone(), flow.clone());
    let confirmed = write_confirm::confirmation(
        core,
        review,
        move |_| {
            asked.begun.set(asked.begun.get() + 1);
            begin
        },
        move |result, _| finished.done.borrow_mut().push(result),
    );
    (flow, confirmed)
}

/// Runs `confirmed` again once its first attempt ended: nothing more may happen.
fn assert_runs_once(
    cx: &mut TestAppContext,
    api: &LocalApi,
    flow: &Flow,
    confirmed: &impl Fn(&mut gpui_kit::App),
) {
    let (begun, done, writes) = (flow.begun.get(), flow.done_count(), api.patches());
    cx.update(|cx| confirmed(cx));
    std::thread::sleep(SETTLE);
    cx.run_until_parked();
    assert_eq!(flow.begun.get(), begun, "begin is not asked again");
    assert_eq!(flow.done_count(), done, "on_done is not called again");
    assert_eq!(api.patches(), writes, "no second write");
}

// covers UP-53: Cancel and Escape send nothing, and report nothing.
#[gpui_kit::test]
async fn cancelling_sends_no_write(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[]);
    let t = connected(cx, dir.path(), &api).await;

    let flow = open(cx, &t, &api, SCALE, true).await;
    t.click(cx, "cancel");
    assert_nothing_ran(cx, &t, &api, &flow);

    let flow = open(cx, &t, &api, SCALE, true).await;
    t.press(cx, "escape");
    assert_nothing_ran(cx, &t, &api, &flow);
}

// covers UP-53: Enter does not run an action that cannot be got back.
#[gpui_kit::test]
async fn enter_sends_no_delete(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[]);
    let t = connected(cx, dir.path(), &api).await;

    let flow = open(cx, &t, &api, WriteProposal::Delete, true).await;
    t.press(cx, "enter");
    assert_nothing_ran(cx, &t, &api, &flow);
}

// covers UP-53: Confirm sends the reviewed write once, and reports it once.
#[gpui_kit::test]
async fn confirming_sends_exactly_one_write(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[]);
    let t = connected(cx, dir.path(), &api).await;

    let flow = open(cx, &t, &api, SCALE, true).await;
    assert_eq!(api.patches(), 0, "nothing runs before Confirm");
    t.click(cx, "ok");
    wait_done(cx, &flow);

    assert_eq!(api.patches(), 1);
    assert_eq!(flow.begun.get(), 1);
    flow.result().expect("the write went through");
    draw(cx, &t);
    assert!(!t.has_dialog(cx));
}

// covers UP-53: Confirm sends the reviewed change to the reviewed object, and nothing before it.
#[gpui_kit::test]
async fn confirming_sends_the_reviewed_change_to_the_reviewed_object(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let server = fake_api();
    let t = connected_to_fake(cx, dir.path(), &server).await;

    let flow = open_on(cx, &t, act(&server), SCALE, true).await;
    assert!(server.writes().is_empty(), "nothing runs before Confirm");
    t.click(cx, "ok");
    wait_done(cx, &flow);
    flow.result().expect("the write went through");

    let write = only_write(&server);
    assert_eq!(write.method.as_str(), "PATCH");
    assert_eq!(write.path, format!("{ACT_PATH}/scale"));
    assert_eq!(write.query, "fieldManager=flint");
    assert_eq!(write.content_type, "application/json-patch+json");
    assert_eq!(
        write.json(),
        Some(json!([
            {"op": "test", "path": "/metadata/uid", "value": "uid-act"},
            {"op": "test", "path": "/spec/replicas", "value": 2},
            {"op": "replace", "path": "/spec/replicas", "value": 5},
        ]))
    );
    assert_eq!(write.status, Some(200));
    let live = server.object(&deployments(), Some("demo"), "act");
    assert_eq!(live.unwrap()["spec"]["replicas"], 5);
}

// covers UP-53: Enter on Delete sends no mutating request of any method; Confirm then sends the
// one delete, bound to the reviewed uid.
#[gpui_kit::test]
async fn enter_on_delete_sends_no_mutating_request(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let server = fake_api();
    let t = connected_to_fake(cx, dir.path(), &server).await;

    let flow = open_on(cx, &t, act(&server), WriteProposal::Delete, true).await;
    t.press(cx, "enter");
    std::thread::sleep(SETTLE);
    draw(cx, &t);
    assert_eq!(flow.begun.get(), 0, "begin is not asked");
    assert_eq!(flow.done_count(), 0, "on_done is not called");
    let writes = server.writes();
    assert!(writes.is_empty(), "no mutating request: {writes:?}");
    assert!(server.object(&deployments(), Some("demo"), "act").is_some());

    if t.has_dialog(cx) {
        t.press(cx, "escape");
        draw(cx, &t);
    }
    let flow = open_on(cx, &t, act(&server), WriteProposal::Delete, true).await;
    t.click(cx, "ok");
    wait_done(cx, &flow);
    flow.result().expect("the delete went through");

    let write = only_write(&server);
    assert_eq!(write.method.as_str(), "DELETE");
    assert_eq!(write.path, ACT_PATH);
    assert!(!write.is_dry_run());
    assert_eq!(write.json().unwrap()["preconditions"]["uid"], "uid-act");
    assert!(server.object(&deployments(), Some("demo"), "act").is_none());
}

// covers UP-53: Confirm twice before the dialog redraws is still one write.
#[gpui_kit::test]
async fn confirming_twice_sends_one_write(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[]);
    let t = connected(cx, dir.path(), &api).await;

    let flow = open(cx, &t, &api, SCALE, true).await;
    cx.update_window(t.window, |_, window, cx| {
        registry::dispatch("ui::Confirm", Value::Null, window, cx).unwrap();
        // The dialog may already be gone, and then nothing answers.
        let _ = registry::dispatch("ui::Confirm", Value::Null, window, cx);
    })
    .unwrap();
    wait_done(cx, &flow);

    assert_eq!(api.patches(), 1);
    assert_eq!(flow.begun.get(), 1);
    flow.result().expect("the write went through");
}

// covers UP-52, UP-53: a failed write is reported once and never sent again, even on a status a
// client's retry policy would retry.
#[gpui_kit::test]
async fn a_failed_write_is_reported_and_not_retried(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[503, 409]);
    let t = connected(cx, dir.path(), &api).await;

    let flow = open(cx, &t, &api, SCALE, true).await;
    t.click(cx, "ok");
    wait_done(cx, &flow);
    assert_eq!(api.patches(), 1);
    let error = flow.result().expect_err("a server error");
    assert!(
        matches!(error, WriteError::Failed(_) | WriteError::Rejected(_)),
        "{error:?}"
    );

    draw(cx, &t);
    let flow = open(cx, &t, &api, SCALE, true).await;
    t.click(cx, "ok");
    wait_done(cx, &flow);
    assert_eq!(api.patches(), 2);
    assert_eq!(flow.result(), Err(WriteError::Conflict));
}

// covers UP-54: confirming does not authorize: a cluster locked while the dialog is open refuses
// the write, and the refusal is reported.
#[gpui_kit::test]
async fn a_refused_write_is_reported_and_not_sent(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[]);
    let t = connected_unlocked(cx, dir.path(), &api).await;
    let core = t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());

    let flow = open(cx, &t, &api, SCALE, true).await;
    assert!(core.lock("alpha"), "it was unlocked");
    t.click(cx, "ok");
    wait_done(cx, &flow);

    assert_eq!(api.patches(), 0, "the gate refused before any write");
    assert_eq!(
        flow.result(),
        Err(WriteError::Refused(WriteRefusal::Protected {
            cluster: "alpha".to_owned()
        }))
    );
}

// covers UP-53: when `begin` says no (another write holds the object), nothing is sent, and the
// caller, which already knows, is not called.
#[gpui_kit::test]
async fn a_refused_begin_sends_no_write(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[]);
    let t = connected(cx, dir.path(), &api).await;

    let flow = open(cx, &t, &api, SCALE, false).await;
    t.click(cx, "ok");
    std::thread::sleep(SETTLE);
    draw(cx, &t);

    assert_eq!(flow.begun.get(), 1);
    assert_eq!(flow.done_count(), 0);
    assert_eq!(api.patches(), 0);
}

// covers UP-53: a write still running when its window closes reports back, so its caller can
// release the object it claimed.
#[gpui_kit::test]
async fn a_write_reports_back_after_its_window_closed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[]);
    let t = connected(cx, dir.path(), &api).await;

    let flow = open(cx, &t, &api, SCALE, true).await;
    api.hold(true);
    t.click(cx, "ok");
    wait_patches(&api, 1);
    cx.update_window(t.window, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert_eq!(flow.done_count(), 0, "it is still running");

    api.hold(false);
    wait_done(cx, &flow);
    assert_eq!(api.patches(), 1);
    flow.result().expect("the write went through");
}

// covers UP-52, UP-53: a confirmation runs once however its first attempt ended; only a new
// review can ask again.
#[gpui_kit::test]
async fn a_confirmation_runs_once_after_any_outcome(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[500, 409]);
    let t = connected(cx, dir.path(), &api).await;

    // `begin` said no.
    let (flow, confirmed) = handler(cx, &t, &api, false).await;
    cx.update(|cx| confirmed(cx));
    assert_eq!((flow.begun.get(), flow.done_count()), (1, 0));
    assert_runs_once(cx, &api, &flow, &confirmed);
    assert_eq!(api.patches(), 0);

    // The server failed it, then found a conflict, then took it.
    for writes in 1..=3 {
        let (flow, confirmed) = handler(cx, &t, &api, true).await;
        cx.update(|cx| confirmed(cx));
        wait_done(cx, &flow);
        assert_eq!(api.patches(), writes);
        assert_eq!(flow.result().is_ok(), writes == 3);
        assert_runs_once(cx, &api, &flow, &confirmed);
    }
}

// covers UP-54: a confirmation the gate refused does not run again once the cluster is unlocked.
#[gpui_kit::test]
async fn a_refused_confirmation_does_not_run_after_an_unlock(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let api = LocalApi::start(&[]);
    let t = connected_unlocked(cx, dir.path(), &api).await;
    let core = t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());

    let (flow, confirmed) = handler(cx, &t, &api, true).await;
    assert!(core.lock("alpha"));
    cx.update(|cx| confirmed(cx));
    wait_done(cx, &flow);
    assert!(matches!(flow.result(), Err(WriteError::Refused(_))));

    core.unlock("alpha", &api.url);
    assert_runs_once(cx, &api, &flow, &confirmed);
    assert_eq!(api.patches(), 0);
}
