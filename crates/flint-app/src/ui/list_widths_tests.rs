//! View tests for steady column widths (#828): a list saves the widths it measured to
//! `state.toml` per cluster and kind, opens at them, and reserves CPU and Memory while the first
//! metrics poll is pending.

use flint_core::settings::STATE_FILE;
use gpui_kit::TestAppContext;
use gpui_kit::test::TestAppContextExt as _;
use serde_json::json;

use super::list_usage::UsageSource;
use super::support_tests::{TestWorkspace, WAIT, open_workspace, pods, standard_sections};
use super::ui_state::UiState;

const PODS: &[(&str, &str, i64)] = &[("demo", "web", 3), ("data", "db", 7)];

/// The open list's columns and their widths in characters.
fn chars(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<(String, usize)> {
    let list = t.list(cx).expect("a list is open");
    list.read_with(cx, |list, _| list.column_chars())
}

fn chars_of(columns: &[(String, usize)], name: &str) -> usize {
    columns
        .iter()
        .find(|(column, _)| column == name)
        .unwrap_or_else(|| panic!("no {name} column in {columns:?}"))
        .1
}

/// Opens the pods list of `kind-alpha` without any rows yet.
async fn open_empty_pods(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.connect_alpha(cx, standard_sections()).await;
    t.dispatch(cx, "workspace::OpenView", json!({ "kind": "pods" }))
        .unwrap();
    cx.run_until_parked();
}

async fn wait_for_saved(cx: &mut TestAppContext, t: &TestWorkspace, saved: fn(&str) -> bool) {
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    let path = t.dir.join(STATE_FILE);
    cx.wait_for(t.window, WAIT, move |_, _| {
        std::fs::read_to_string(&path).is_ok_and(|s| saved(&s))
    })
    .await;
}

#[gpui_kit::test]
async fn a_list_opens_at_saved_widths_and_saves_the_fresh_measurement(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // A context name as EKS writes it: colons and slashes in the table key.
    std::fs::write(
        dir.path().join(STATE_FILE),
        "[list_widths.kind-alpha.pods]\n\
         \"flint:name\" = 40\n\
         \"flint:namespace\" = 20\n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_empty_pods(cx, &t).await;
    // The first frame with columns, before any row arrives, is at the saved widths.
    t.feed_list(cx, pods(&[])).await;
    let first = chars(cx, &t);
    assert_eq!(chars_of(&first, "Name"), 40);
    assert_eq!(chars_of(&first, "Namespace"), 20);

    t.feed_list(cx, pods(PODS)).await;
    // The measurement folds in grow-only: the saved widths hold while the list is open.
    let shown = chars(cx, &t);
    assert_eq!(chars_of(&shown, "Name"), 40);
    assert_eq!(chars_of(&shown, "Namespace"), 20);

    // What is saved is the fresh measurement, so a column can shrink between opens.
    wait_for_saved(cx, &t, |s| s.contains("flint:name") && !s.contains("= 40")).await;
    let saved = UiState::load(dir.path().to_owned()).list_widths("kind-alpha", "pods");
    assert!(saved["flint:name"] < 20, "{saved:?}");
    assert!(saved["flint:namespace"] < 20, "{saved:?}");
    assert!(
        UiState::load(dir.path().to_owned())
            .list_widths("kind-alpha", "nodes")
            .is_empty()
    );
}

#[gpui_kit::test]
async fn saved_cpu_and_memory_columns_are_reserved_until_the_first_poll(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(STATE_FILE),
        "[list_widths.kind-alpha.pods]\n\
         \"flint:name\" = 30\n\
         \"flint:cpu-usage\" = 16\n\
         \"flint:memory-usage\" = 16\n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_empty_pods(cx, &t).await;
    // The first poll is pending, as in a cluster that is watched.
    let list = t.list(cx).expect("a list is open");
    list.update(cx, |list, cx| {
        list.pin_usage_source(UsageSource::Hidden, cx)
    });
    t.feed_list(cx, pods(&[])).await;
    let names = |columns: &[(String, usize)]| -> Vec<String> {
        columns.iter().map(|(name, _)| name.clone()).collect()
    };
    let first = chars(cx, &t);
    assert_eq!(&names(&first)[names(&first).len() - 2..], ["CPU", "Memory"]);
    assert_eq!(chars_of(&first, "CPU"), 16);

    // Rows arriving before the poll answers keep the columns, blank.
    t.feed_list(cx, pods(PODS)).await;
    let waiting = chars(cx, &t);
    assert_eq!(names(&waiting), names(&first));
    assert_eq!(chars_of(&waiting, "CPU"), 16);

    // Metrics turn out not to be installed: the columns go, once.
    list.update(cx, |list, cx| {
        list.pin_usage_source(UsageSource::NotInstalled, cx)
    });
    cx.run_until_parked();
    let gone = chars(cx, &t);
    assert!(!names(&gone).contains(&"CPU".to_owned()), "{gone:?}");
}

#[gpui_kit::test]
async fn cpu_and_memory_are_not_reserved_without_both_saved_keys(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(STATE_FILE),
        "[list_widths.kind-alpha.pods]\n\"flint:cpu-usage\" = 16\n",
    )
    .unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_empty_pods(cx, &t).await;
    assert!(
        !chars(cx, &t).iter().any(|(name, _)| name == "CPU"),
        "no reservation from one key"
    );
}
