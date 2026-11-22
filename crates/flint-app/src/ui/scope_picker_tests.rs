//! Tests for the AI scope pickers (`scope_picker.rs`): what they offer, how checking works,
//! and the summary the sidebar and dialogs show.

use flint_core::settings::AiScope;
use gpui_kit::{AppContext as _, TestAppContext};

use super::{Mode, ScopePicker, added_access, is_namespace_name, short_summary, summary, toggle};
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
        assert_eq!(p.scope(), Some(saved.clone()));
    });
    // Events are not offered: they follow the namespaces and kinds chosen.
    let mut with_events = kinds.clone();
    let mut event = kinds[0].clone();
    event.resource.plural = "events".into();
    event.resource.kind = "Event".into();
    with_events.push(event.clone());
    event.resource.group = "events.k8s.io".into();
    with_events.push(event);
    let picker = window.update(|window, cx| {
        cx.new(|cx| ScopePicker::new(&[], &with_events, &AiScope::default(), window, cx))
    });
    picker.read_with(window, |p, _| {
        let (_, kinds) = p.offered();
        assert!(
            kinds.iter().all(|k| !k.id.starts_with("events")),
            "{kinds:?}"
        );
    });
}

// covers UP-14: only a namespace name is taken from the field.
#[test]
fn typed_namespaces_must_be_namespace_names() {
    for ok in ["demo", "kube-system", "a1"] {
        assert!(is_namespace_name(ok), "{ok}");
    }
    for bad in ["", "Demo", "-a", "a-", "a/b", "a b", "a.b", &"a".repeat(64)] {
        assert!(!is_namespace_name(bad), "{bad}");
    }
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

// The note beside the buttons names a short list and counts a long one.
#[test]
fn the_short_summary_stays_short() {
    assert_eq!(
        short_summary(&scope(&["shop"], &["pods"])),
        "namespaces shop; kinds pods"
    );
    let long = "n".repeat(63);
    assert_eq!(
        short_summary(&scope(&[&long], &[])),
        "1 selected namespace; all namespaced kinds"
    );
    let many: Vec<String> = (0..20).map(|i| format!("namespace-{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(
        short_summary(&scope(&many, &["pods"])),
        "20 selected namespaces; kinds pods"
    );
}

// A scope saved as All (empty on disk) loads as All, and stays All when saved unchanged.
#[gpui_kit::test]
fn a_saved_all_scope_loads_as_all(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let window = cx.add_empty_window();
    let picker = window.update(|window, cx| {
        cx.new(|cx| ScopePicker::new(&["shop".into()], &[], &AiScope::default(), window, cx))
    });
    picker.read_with(window, |p, cx| {
        assert_eq!((p.namespace_mode, p.kind_mode), (Mode::All, Mode::All));
        assert_eq!(p.scope(), Some(AiScope::default()));
        assert!(p.added_access(&AiScope::default()).is_empty());
        let state = p.confirm_state(cx);
        assert!(!state.blocked);
        assert_eq!(state.note, "Agents may read all namespaces; all kinds.");
    });
}

// Selected with nothing chosen has no scope to save: it never reads as All.
#[gpui_kit::test]
fn selected_with_nothing_chosen_is_refused(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let window = cx.add_empty_window();
    let saved = scope(&["shop"], &["pods"]);
    let picker =
        window.update(|window, cx| cx.new(|cx| ScopePicker::new(&[], &[], &saved, window, cx)));
    picker.update(window, |p, _| {
        p.selected_kinds.clear();
    });
    picker.read_with(window, |p, cx| {
        assert_eq!(p.kind_mode, Mode::Selected);
        assert_eq!(p.scope(), None);
        let state = p.confirm_state(cx);
        assert!(state.blocked);
        assert_eq!(state.note, "Select a kind, or choose All kinds.");
    });
    window.update(|window, cx| {
        picker.update(cx, |p, cx| {
            assert_eq!(p.confirmed_scope(window, cx), Err(()));
        })
    });
}

// Whatever the change, a scope that can read anything the saved one cannot is reported, and a
// strictly narrower one is not.
#[test]
fn added_access_matches_what_agents_could_read() {
    let namespaces: [&[&str]; 4] = [&[], &["a"], &["b"], &["a", "b"]];
    let kinds: [&[&str]; 4] = [&[], &["pods"], &["nodes"], &["nodes", "pods"]];
    let scopes: Vec<AiScope> = namespaces
        .iter()
        .flat_map(|n| kinds.iter().map(|k| scope(n, k)))
        .collect();
    let namespaced = ["pods".to_owned(), "deployments".to_owned()]
        .into_iter()
        .collect();
    let reads = |s: &AiScope, ns: &str, kind: &str, is_namespaced: bool| {
        s.allows_kind(kind, is_namespaced) && (!is_namespaced || s.allows_namespace(ns))
    };
    for saved in &scopes {
        for next in &scopes {
            let widens = ["a", "b", "c"].iter().any(|ns| {
                [("pods", true), ("deployments", true), ("nodes", false)]
                    .iter()
                    .any(|(k, n)| reads(next, ns, k, *n) && !reads(saved, ns, k, *n))
            });
            let added = added_access(saved, next, &namespaced);
            assert!(!widens || !added.is_empty(), "{saved:?} to {next:?}");
            // Narrower or equal lists add nothing. Other changes may be reported even when no
            // kind tried here shows it: anything not strictly narrower counts as broadening.
            let within = |next: &[String], saved: &[String]| {
                saved.is_empty() || (!next.is_empty() && next.iter().all(|n| saved.contains(n)))
            };
            if within(&next.namespaces, &saved.namespaces) && within(&next.kinds, &saved.kinds) {
                let cluster_wide = saved.kinds.is_empty()
                    && !saved.namespaces.is_empty()
                    && next.kinds.iter().any(|k| k == "nodes");
                assert_eq!(added.is_empty(), !cluster_wide, "{saved:?} to {next:?}");
            }
        }
    }
    // A kind discovery does not know may be cluster-wide.
    assert_eq!(
        added_access(
            &scope(&["a"], &[]),
            &scope(&["a"], &["gizmos"]),
            &namespaced
        ),
        ["cluster-wide kinds gizmos"]
    );
    assert!(added_access(&scope(&["a"], &[]), &scope(&["a"], &["pods"]), &namespaced).is_empty());
}
