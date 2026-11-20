//! Tests for the AI scope pickers (`scope_picker.rs`): what they offer, how checking works,
//! and the summary the sidebar and dialogs show.

use flint_core::settings::AiScope;
use gpui_kit::{AppContext as _, TestAppContext};

use super::{ScopePicker, summary, toggle};
use crate::ui::support_tests::{discovery, standard_sections};

fn scope(namespaces: &[&str], kinds: &[&str]) -> AiScope {
    AiScope::new(
        namespaces.iter().map(|s| (*s).to_owned()).collect(),
        kinds.iter().map(|s| (*s).to_owned()).collect(),
    )
}

// covers UP-14: the pickers offer the cluster's namespaces and listable kinds, plus what the
// scope names that the cluster does not offer now, with the scope checked.
#[gpui_kit::test]
fn the_pickers_offer_discovery_and_the_scope(cx: &mut TestAppContext) {
    let kinds = discovery(standard_sections()).kinds;
    let saved = scope(&["gone"], &["pods", "gizmos.example.org"]);
    cx.update(gpui_kit::init);
    let window = cx.add_empty_window();
    let picker = window.update(|window, cx| {
        cx.new(|cx| ScopePicker::new(&["shop".into(), "demo".into()], &kinds, &saved, window, cx))
    });
    picker.read_with(window, |p, _| {
        let (namespaces, kinds) = p.offered();
        assert_eq!(namespaces, ["demo", "gone", "shop"]);
        let ids: Vec<&str> = kinds.iter().map(|k| k.id.as_str()).collect();
        for id in ["pods", "deployments.apps", "nodes", "gizmos.example.org"] {
            assert!(ids.contains(&id), "{ids:?}");
        }
        let nodes = kinds.iter().find(|k| k.id == "nodes").unwrap();
        assert!(!nodes.namespaced);
        let deployments = kinds.iter().find(|k| k.id == "deployments.apps").unwrap();
        assert_eq!(deployments.label, "Deployment (apps)");
        assert_eq!(p.scope(), saved);
    });
}

// covers UP-14
#[test]
fn toggling_adds_and_removes() {
    let mut set = std::collections::BTreeSet::new();
    toggle(&mut set, "shop");
    toggle(&mut set, "demo");
    toggle(&mut set, "shop");
    toggle(&mut set, "");
    assert_eq!(set.into_iter().collect::<Vec<_>>(), ["demo"]);
}

// covers UP-14
#[test]
fn summaries_say_what_agents_may_read() {
    assert_eq!(summary(&AiScope::default()), "all namespaces; all kinds");
    assert_eq!(
        summary(&scope(&["shop", "billing"], &[])),
        "namespaces billing, shop; all namespaced kinds"
    );
    assert_eq!(
        summary(&scope(&[], &["pods", "nodes"])),
        "all namespaces; kinds nodes, pods"
    );
}
