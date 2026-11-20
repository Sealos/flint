//! View tests for the resource switcher (Cmd-P, UP-31): opening it with its key, the kinds and
//! the objects in memory with their kind and cluster, filtering, opening a kind or an object,
//! that it never reaches the cluster, an empty cluster, switching to the actions and back, and
//! its release once closed.

use flint_core::handle::StoredForTests;
use flint_core::{KindId, SyncState};
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, Entity, TestAppContext};

use super::{CommandPalette, Mode};
use crate::ui::support_tests::{
    TestWorkspace, WAIT, objects, open_workspace, standard_sections, weaken,
};
use crate::ui::view_state::{Focus, PaletteState};

/// Puts rows in the core's stores for `alpha`, as its watches would hold them.
fn store(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    kind: KindId,
    rows: &[(&str, &str, i64)],
) -> StoredForTests {
    let snapshot = (*objects(kind, rows, SyncState::Synced, 1)).clone();
    t.model
        .read_with(cx, |m, _| m.core().store_for_tests(snapshot))
}

/// `alpha` connected with the standard navigation, and pods and a deployment in memory.
async fn alpha_with_objects(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<StoredForTests> {
    t.connect_alpha(cx, standard_sections()).await;
    vec![
        store(
            cx,
            t,
            KindId::pods(),
            &[("demo", "web-7d9f", 0), ("kube-system", "coredns-1", 0)],
        ),
        store(
            cx,
            t,
            KindId::new("apps", "deployments"),
            &[("demo", "web", 0)],
        ),
    ]
}

fn palette(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<Entity<CommandPalette>> {
    t.workspace.read_with(cx, |ws, _| ws.palette_view())
}

/// Opens the switcher with its key and waits until it has read the objects in memory.
async fn open_switcher(cx: &mut TestAppContext, t: &TestWorkspace) -> Entity<CommandPalette> {
    t.press_key_of(cx, "palette::ToggleResourceSwitcher");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).palette_view().is_some_and(|p| {
            let p = p.read(cx);
            p.mode() == Mode::Resources && p.resources.as_ref().is_some_and(|r| !r.loading)
        })
    })
    .await;
    palette(cx, t).expect("the switcher is open")
}

fn state(cx: &mut TestAppContext, t: &TestWorkspace) -> (Option<PaletteState>, Focus) {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        window.render_frame(cx);
        workspace.update(cx, |ws, cx| {
            let state = ws.view_state(window, cx);
            (state.palette, state.focus)
        })
    })
    .unwrap()
}

fn shown(cx: &mut TestAppContext, t: &TestWorkspace) -> PaletteState {
    state(cx, t).0.expect("the switcher is open")
}

fn labels(state: &PaletteState) -> Vec<&str> {
    state.rows.iter().map(|r| r.label.as_str()).collect()
}

// covers UP-31: Cmd-P opens the switcher over the kinds the cluster serves, then the objects in
// memory, each row with its kind and cluster; typing filters loosely; Esc closes it and gives
// focus back.
#[gpui_kit::test]
async fn lists_kinds_and_objects_and_filters_them(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _stored = alpha_with_objects(cx, &t).await;
    t.focus_workspace(cx);

    open_switcher(cx, &t).await;
    let (switcher, focus) = state(cx, &t);
    let switcher = switcher.unwrap();
    assert_eq!(focus, Focus::Palette);
    assert_eq!(switcher.mode, "resources");
    assert_eq!(
        labels(&switcher),
        [
            "Pods",
            "Deployments",
            "Services",
            "Secrets",
            "Nodes",
            "Widgets",
            "demo/web",
            "demo/web-7d9f",
            "kube-system/coredns-1",
        ]
    );
    let pods = &switcher.rows[0];
    assert_eq!(pods.detail.as_deref(), Some("Kind · 2 in memory"));
    assert_eq!(
        switcher.rows[2].detail.as_deref(),
        Some("Kind · not loaded"),
        "Services: no watch has listed them"
    );
    assert_eq!(
        switcher.note, None,
        "watch mode All lists every kind's objects"
    );
    assert_eq!(pods.kind.as_deref(), Some("pods"));
    assert_eq!(pods.cluster.as_deref(), Some("alpha"));
    let web = &switcher.rows[6];
    assert_eq!(web.detail.as_deref(), Some("Deployments"));
    assert_eq!(web.kind.as_deref(), Some("deployments.apps"));
    assert_eq!(web.cluster.as_deref(), Some("alpha"));
    assert_eq!(
        switcher.footer.as_deref(),
        Some("6 kinds and 3 objects in memory")
    );

    t.type_text(cx, "coredns");
    assert_eq!(labels(&shown(cx, &t)), ["kube-system/coredns-1"]);
    t.type_text(cx, "zz");
    let switcher = shown(cx, &t);
    assert!(switcher.rows.is_empty());
    assert_eq!(switcher.footer.as_deref(), Some("Nothing matches"));

    t.press(cx, "escape");
    assert!(palette(cx, &t).is_none());
    assert_eq!(state(cx, &t).1, Focus::Workspace);
}

// covers UP-31: "pods web" finds the pod by its kind and name, and the arrows move the
// selection as in the palette.
#[gpui_kit::test]
async fn finds_objects_by_kind_and_name(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _stored = alpha_with_objects(cx, &t).await;
    open_switcher(cx, &t).await;
    t.type_text(cx, "pods web");
    assert_eq!(labels(&shown(cx, &t)), ["demo/web-7d9f"]);
    for _ in 0.."pods web".len() {
        t.press(cx, "backspace");
    }
    t.type_text(cx, "web");
    let switcher = shown(cx, &t);
    assert_eq!(labels(&switcher), ["demo/web", "demo/web-7d9f"]);
    assert_eq!(switcher.selected, Some(0));
    t.press(cx, "down");
    assert_eq!(shown(cx, &t).selected, Some(1));
    t.press(cx, "up");
    assert_eq!(shown(cx, &t).selected, Some(0));
}

// covers UP-31: when the objects arrive (or arrive again), the selection stays on the item the
// user moved to, wherever its row went; an item no longer listed gives the first row.
#[gpui_kit::test]
async fn the_selection_follows_its_item_when_objects_arrive(cx: &mut TestAppContext) {
    use crate::ui::switcher_model::Index;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _stored = alpha_with_objects(cx, &t).await;
    let switcher = open_switcher(cx, &t).await;
    t.type_text(cx, "web");
    t.press(cx, "down");
    let switcher_state = shown(cx, &t);
    assert_eq!(selected(&switcher_state), "demo/web-7d9f");
    let entries: Vec<flint_core::NavEntry> = standard_sections()
        .into_iter()
        .flat_map(|s| s.entries)
        .collect();
    let index = |rows: &[(&str, &str, i64)]| {
        std::sync::Arc::new(Index::new(
            &entries,
            &[objects(KindId::pods(), rows, SyncState::Synced, 2)],
        ))
    };
    // Shorter names arrive and rank first: the selection stays on web-7d9f, two rows down.
    let arrived = index(&[
        ("demo", "web", 0),
        ("demo", "web-1", 0),
        ("demo", "web-7d9f", 0),
    ]);
    switcher.update(cx, |p, cx| p.set_index(arrived, cx));
    let after = shown(cx, &t);
    assert_eq!(labels(&after), ["demo/web", "demo/web-1", "demo/web-7d9f"]);
    assert_eq!(after.selected, Some(2));
    // Gone: the first row.
    let gone = index(&[("demo", "web", 0)]);
    switcher.update(cx, |p, cx| p.set_index(gone, cx));
    assert_eq!(shown(cx, &t).selected, Some(0));
}

fn selected(state: &PaletteState) -> &str {
    &state.rows[state.selected.expect("a row is selected")].label
}

// covers UP-31: Enter on a kind opens its list, with focus, and closes the switcher.
#[gpui_kit::test]
async fn enter_on_a_kind_opens_its_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _stored = alpha_with_objects(cx, &t).await;
    open_switcher(cx, &t).await;
    t.type_text(cx, "servic");
    assert_eq!(labels(&shown(cx, &t)), ["Services"]);
    t.press(cx, "enter");
    assert!(palette(cx, &t).is_none());
    let list = t.list(cx).expect("a list is open");
    assert_eq!(
        list.read_with(cx, |l, _| l.kind().clone()),
        KindId::new("", "services")
    );
    assert_eq!(state(cx, &t).1, Focus::List);
}

// covers UP-31: Enter on an object opens its kind's list with it selected and its details
// shown. A namespace filter that hides it is left alone (nothing the user did not ask for is
// saved): the list says the object is filtered out.
#[gpui_kit::test]
async fn enter_on_an_object_opens_it_selected_with_its_details(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _stored = alpha_with_objects(cx, &t).await;
    t.dispatch(
        cx,
        "workspace::SetNamespaces",
        serde_json::json!({ "namespaces": ["kube-system"] }),
    )
    .unwrap();
    open_switcher(cx, &t).await;
    t.type_text(cx, "web-7d");
    assert_eq!(labels(&shown(cx, &t)), ["demo/web-7d9f"]);
    t.press(cx, "enter");
    assert!(palette(cx, &t).is_none());
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).detail_view().is_some()
    })
    .await;
    let list = t.list(cx).unwrap();
    assert_eq!(list.read_with(cx, |l, _| l.kind().clone()), KindId::pods());
    assert_eq!(t.selected(cx).as_deref(), Some("web-7d9f"));
    let detail = t.detail(cx).unwrap();
    let (ns, name) = detail.read_with(cx, |d, _| {
        (d.object().namespace.clone(), d.object().name.clone())
    });
    assert_eq!((ns.as_deref(), name.as_str()), (Some("demo"), "web-7d9f"));
    let (list_state, focus) = {
        let workspace = t.workspace.clone();
        cx.update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            workspace.update(cx, |ws, cx| {
                let state = ws.view_state(window, cx);
                (state.list.unwrap(), state.focus)
            })
        })
        .unwrap()
    };
    assert_eq!(focus, Focus::List);
    let filter = t
        .workspace
        .read_with(cx, |ws, _| ws.namespace_filter_of(&"alpha".into()));
    assert_eq!(filter, ["kube-system"], "the filter is not changed");
    let banner = list_state
        .banners
        .iter()
        .find(|b| b.id == "list-filtered-out")
        .expect("the list says the object is filtered out");
    assert!(
        banner.text.contains("web-7d9f is in namespace demo"),
        "{banner:?}"
    );
    assert!(t.find(cx, "list-filtered-out").is_some());
    // Picking a row the filter shows ends it.
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        serde_json::json!({"namespace": "kube-system", "name": "coredns-1"}),
    )
    .unwrap();
    assert!(t.find(cx, "list-filtered-out").is_none());
}

// covers UP-31: searching reads memory only: no connection reaches the cluster, and no
// kind is subscribed (which would start a watch in On view mode).
#[gpui_kit::test]
async fn searching_never_reaches_the_cluster(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _stored = alpha_with_objects(cx, &t).await;
    // Whatever connecting alpha did, before the switcher opens.
    while t.server_was_contacted() {}
    let core = t.model.read_with(cx, |m, _| m.core().clone());
    let hubs = core.sync_states("alpha");

    open_switcher(cx, &t).await;
    for query in ["w", "we", "web", "pods", "coredns"] {
        t.type_text(cx, query);
        assert!(shown(cx, &t).total > 0, "{query}");
        for _ in 0..query.len() {
            t.press(cx, "backspace");
        }
    }
    t.press(cx, "escape");
    open_switcher(cx, &t).await;
    t.press(cx, "escape");
    cx.run_until_parked();

    assert!(
        !t.server_was_contacted(),
        "the switcher reached the cluster"
    );
    assert_eq!(core.sync_states("alpha"), hubs, "a kind was subscribed");
}

// covers UP-31: a cluster with nothing in memory lists its kinds; with no cluster active the
// switcher says to select one; before discovery, that there are no kinds yet.
#[gpui_kit::test]
async fn an_empty_cluster_lists_only_its_kinds(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_switcher(cx, &t).await;
    let switcher = shown(cx, &t);
    assert!(switcher.rows.is_empty());
    assert_eq!(
        switcher.footer.as_deref(),
        Some("No cluster is active; select one first")
    );
    t.press(cx, "escape");

    t.activate(cx, "alpha").await;
    open_switcher(cx, &t).await;
    assert_eq!(
        shown(cx, &t).footer.as_deref(),
        Some("No kinds (discovery has not finished)")
    );
    t.press(cx, "escape");

    t.connect_alpha(cx, standard_sections()).await;
    open_switcher(cx, &t).await;
    let switcher = shown(cx, &t);
    assert_eq!(switcher.total, 6);
    assert!(
        switcher
            .rows
            .iter()
            .all(|r| r.detail.as_deref() == Some("Kind · not loaded"))
    );
    assert_eq!(
        switcher.footer.as_deref(),
        Some("6 kinds and 0 objects in memory")
    );
    t.type_text(cx, "web");
    assert_eq!(shown(cx, &t).footer.as_deref(), Some("Nothing matches"));
}

// covers UP-31: a failed discovery is said as such, and in On view mode the switcher says
// that only the kinds being watched list their objects.
#[gpui_kit::test]
async fn says_why_kinds_or_objects_are_missing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.activate(cx, "alpha").await;
    t.feed(
        cx,
        flint_core::Event::DiscoveryChanged {
            cluster: "alpha".into(),
            discovery: None,
            error: Some("the server is unreachable".into()),
        },
    );
    open_switcher(cx, &t).await;
    assert_eq!(
        shown(cx, &t).footer.as_deref(),
        Some("No kinds: discovery failed")
    );
    t.press(cx, "escape");

    let _stored = alpha_with_objects(cx, &t).await;
    t.dispatch(cx, "workspace::WatchOpenViewsOnly", serde_json::Value::Null)
        .unwrap();
    let model = t.model.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        model.read(cx).settings.watch_mode_for("alpha") == flint_core::WatchMode::OnView
    })
    .await;
    open_switcher(cx, &t).await;
    let switcher = shown(cx, &t);
    assert_eq!(
        switcher.note.as_deref(),
        Some("Only kinds being watched list their objects; open a kind to load them.")
    );
    assert!(t.find(cx, "palette-note").is_some());
}

// covers UP-31, UP-33: opening an object that is gone clears the selection shown before, says
// the object no longer exists once the list is current, and stops waiting: one made later
// with the name is not selected.
#[gpui_kit::test]
async fn opening_a_gone_object_says_so_and_stops_waiting(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        serde_json::json!({"namespace": "demo", "name": "api"}),
    )
    .unwrap();
    assert!(t.detail(cx).is_some());
    t.dispatch(
        cx,
        "workspace::OpenObject",
        serde_json::json!({"kind": "pods", "namespace": "demo", "name": "web"}),
    )
    .unwrap();
    assert_eq!(t.selected(cx), None, "the old selection is cleared at once");
    assert!(t.detail(cx).is_none());
    let list_state = |cx: &mut TestAppContext| {
        let workspace = t.workspace.clone();
        cx.update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            workspace.update(cx, |ws, cx| ws.view_state(window, cx).list.unwrap())
        })
        .unwrap()
    };
    let banner = list_state(cx)
        .banners
        .into_iter()
        .find(|b| b.id == "list-missing")
        .expect("the list says the object is gone");
    assert_eq!(banner.text, "web no longer exists in demo.");
    // Made again later: not selected out of the blue.
    t.feed_list(
        cx,
        crate::ui::support_tests::pods(&[("demo", "api", 0), ("demo", "web", 0)]),
    )
    .await;
    assert_eq!(t.selected(cx), None);
    // A selection ends the notice.
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        serde_json::json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    assert!(
        list_state(cx)
            .banners
            .iter()
            .all(|b| b.id != "list-missing"),
        "the notice went"
    );
}

// covers UP-30, UP-31: Show Command Palette in the switcher shows the actions, and Go to
// Resource the kinds and objects again; Go to Resource again closes it. Run through the
// registry, as the View menu does: the switcher's key is not the same on every system in the
// palette's field (see the next test).
#[gpui_kit::test]
async fn the_palette_and_the_switcher_swap(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _stored = alpha_with_objects(cx, &t).await;
    t.focus_workspace(cx);
    open_switcher(cx, &t).await;
    t.dispatch(cx, "palette::ToggleCommandPalette", serde_json::Value::Null)
        .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).palette_view().is_some_and(|p| {
            let p = p.read(cx);
            p.mode() == Mode::Actions && !p.rows.is_empty()
        })
    })
    .await;
    let actions = shown(cx, &t);
    assert_eq!(actions.mode, "actions");
    // Listed from where focus was before the switcher opened.
    assert!(labels(&actions).contains(&"Go to Resource"), "{actions:?}");
    t.dispatch(
        cx,
        "palette::ToggleResourceSwitcher",
        serde_json::Value::Null,
    )
    .unwrap();
    assert_eq!(shown(cx, &t).mode, "resources");
    t.dispatch(
        cx,
        "palette::ToggleResourceSwitcher",
        serde_json::Value::Null,
    )
    .unwrap();
    assert!(palette(cx, &t).is_none());
    assert_eq!(state(cx, &t).1, Focus::Workspace);
}

// covers UP-30, UP-31: the switcher's key in the palette's field. On macOS, Cmd-P switches to
// the switcher. Elsewhere the key is Ctrl-P, which the palette's field takes first: it moves
// the selection up, as Ctrl-N moves it down, and the palette stays as it is.
#[gpui_kit::test]
async fn the_switcher_key_in_the_palette_field(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _stored = alpha_with_objects(cx, &t).await;
    t.focus_workspace(cx);
    t.press_key_of(cx, "palette::ToggleCommandPalette");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace
            .read(cx)
            .palette_view()
            .is_some_and(|p| !p.read(cx).rows.is_empty())
    })
    .await;
    t.press(cx, "down");
    t.press(cx, "down");
    assert_eq!(shown(cx, &t).selected, Some(2));
    // `secondary-p`: Cmd-P on macOS, Ctrl-P elsewhere.
    t.press_key_of(cx, "palette::ToggleResourceSwitcher");
    let after = shown(cx, &t);
    if cfg!(target_os = "macos") {
        assert_eq!(after.mode, "resources");
    } else {
        assert_eq!(after.mode, "actions");
        assert_eq!(after.selected, Some(1), "Ctrl-P moved the selection up");
    }
}

// The switcher is released once closed, its field and index included.
#[gpui_kit::test]
async fn a_closed_switcher_is_released(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _stored = alpha_with_objects(cx, &t).await;
    let switcher = open_switcher(cx, &t).await;
    t.type_text(cx, "web");
    let query = switcher.read_with(cx, |p, _| p.query.clone());
    let index = switcher.read_with(cx, |p, _| {
        std::sync::Arc::downgrade(&p.resources.as_ref().unwrap().index)
    });
    let (switcher, query) = (weaken(switcher), weaken(query));
    t.press(cx, "escape");
    cx.run_until_parked();
    switcher.assert_released();
    query.assert_released();
    assert!(index.upgrade().is_none(), "the index is dropped");
}

// covers UP-31: a click drawn on the rows before the objects arrived is dropped, so it cannot
// open the object now in that row; a click on the rows shown opens theirs.
#[gpui_kit::test]
async fn a_click_on_replaced_switcher_rows_is_dropped(cx: &mut TestAppContext) {
    use crate::ui::switcher_model::Index;

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _stored = alpha_with_objects(cx, &t).await;
    let switcher = open_switcher(cx, &t).await;
    t.type_text(cx, "web");
    assert_eq!(labels(&shown(cx, &t)), ["demo/web", "demo/web-7d9f"]);
    let drawn = switcher.read_with(cx, |p, _| p.rows_version);
    // New objects arrive: a pod `web-1` now sits in row 1.
    let entries: Vec<flint_core::NavEntry> = standard_sections()
        .into_iter()
        .flat_map(|s| s.entries)
        .collect();
    let index = std::sync::Arc::new(Index::new(
        &entries,
        &[objects(
            KindId::pods(),
            &[
                ("demo", "web", 0),
                ("demo", "web-1", 0),
                ("demo", "web-7d9f", 0),
            ],
            SyncState::Synced,
            2,
        )],
    ));
    switcher.update(cx, |p, cx| p.set_index(index, cx));
    assert_eq!(
        labels(&shown(cx, &t)),
        ["demo/web", "demo/web-1", "demo/web-7d9f"]
    );
    // The click on `web-7d9f` (row 1 when drawn) arrives now: dropped.
    t.dispatch(
        cx,
        "palette::PalettePick",
        serde_json::json!({"row": 1, "rows": drawn}),
    )
    .unwrap();
    assert!(palette(cx, &t).is_some(), "nothing ran");
    assert!(t.list(cx).is_none(), "no list opened");
    // On the rows shown, row 2 is `web-7d9f`.
    t.click(cx, ("palette-row", 2usize));
    assert!(palette(cx, &t).is_none());
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).detail_view().is_some()
    })
    .await;
    assert_eq!(t.selected(cx).as_deref(), Some("web-7d9f"));
}

// covers UP-31: a kind viewed before keeps its last list; an object made since is not taken
// for gone on that old list, and opens once the list is current. Clearing the selection
// meanwhile drops the pick.
#[gpui_kit::test]
async fn an_object_missing_only_from_an_old_list_opens(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    // The old list, as a watch published it before: no `web`.
    let _old = store(cx, &t, KindId::pods(), &[("demo", "api", 0)]);
    let current = || {
        objects(
            KindId::pods(),
            &[("demo", "api", 0), ("demo", "web", 0)],
            SyncState::Synced,
            u64::MAX / 2,
        )
    };
    let missing = |cx: &mut TestAppContext| {
        let workspace = t.workspace.clone();
        cx.update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            workspace.update(cx, |ws, cx| {
                ws.view_state(window, cx)
                    .list
                    .is_some_and(|l| l.banners.iter().any(|b| b.id == "list-missing"))
            })
        })
        .unwrap()
    };
    let open_web = |cx: &mut TestAppContext| {
        t.dispatch(
            cx,
            "workspace::OpenObject",
            serde_json::json!({"kind": "pods", "namespace": "demo", "name": "web"}),
        )
        .unwrap();
    };
    open_web(cx);
    assert!(!missing(cx), "the old list does not prove it gone");
    assert_eq!(t.selected(cx), None);
    let fresh = current();
    t.feed_list(cx, fresh).await;
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    assert!(!missing(cx));

    // Again on an old list, then Esc: the pick is dropped, and the current list selects
    // nothing.
    t.dispatch(
        cx,
        "workspace::OpenView",
        serde_json::json!({"kind": "services"}),
    )
    .unwrap();
    open_web(cx);
    t.dispatch(cx, "resource_list::CloseDetail", serde_json::Value::Null)
        .unwrap();
    let fresh = current();
    t.feed_list(cx, fresh).await;
    assert_eq!(t.selected(cx), None, "the cleared pick stayed armed");
}
