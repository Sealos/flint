//! View tests for the command palette (UP-30): opening it with its key, filtering, running an
//! action from where focus was, the argument steps, listing only what is available, shortcut
//! hints, recent actions first, Esc, clicks, and its automation state.

use gpui_kit::component::WindowExt as _;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use serde_json::{Value, json};

use super::{ArgInput, Choice, CommandPalette, arg_value, refusal};
use crate::ui::registry::{self, ActionError, ArgKind};
use crate::ui::support_tests::{TestWorkspace, WAIT, open_workspace, weaken};
use crate::ui::view_state::{Focus, PaletteState};

/// Opens the palette with its key, where focus is, and waits until it lists actions.
async fn open_palette(cx: &mut TestAppContext, t: &TestWorkspace) -> Entity<CommandPalette> {
    t.press_key_of(cx, "palette::ToggleCommandPalette");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace
            .read(cx)
            .palette_view()
            .is_some_and(|p| !p.read(cx).rows.is_empty())
    })
    .await;
    palette(cx, t).expect("the palette is open")
}

fn palette(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<Entity<CommandPalette>> {
    t.workspace.read_with(cx, |ws, _| ws.palette_view())
}

/// The palette's state, as the automation channel reports it, with the focus.
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
    state(cx, t).0.expect("the palette is open")
}

fn labels(state: &PaletteState) -> Vec<&str> {
    state.rows.iter().map(|r| r.label.as_str()).collect()
}

/// The label of the selected row.
fn selected(state: &PaletteState) -> &str {
    &state.rows[state.selected.expect("a row is selected")].label
}

// covers UP-30: Cmd-Shift-P opens the palette with focus in its field; typing filters; Esc
// closes it and gives focus back to where it was.
#[gpui_kit::test]
async fn opens_filters_and_closes_with_esc(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    let list = t.list(cx).unwrap();
    let list_focus = list.read_with(cx, |l, _| l.focus_handle().clone());
    cx.update_window(t.window, |_, window, cx| window.focus(&list_focus, cx))
        .unwrap();

    open_palette(cx, &t).await;
    let (palette, focus) = state(cx, &t);
    let palette = palette.unwrap();
    assert_eq!(focus, Focus::Palette);
    assert_eq!(palette.query, "");
    assert_eq!(palette.step, None);
    assert_eq!(palette.selected, Some(0));
    assert!(palette.total > 20, "{palette:?}");
    // The selected action says what it does.
    assert!(palette.footer.is_some());

    t.type_text(cx, "focus fil");
    let palette = shown(cx, &t);
    assert_eq!(palette.query, "focus fil");
    assert_eq!(selected(&palette), "Focus Filter");
    assert!(
        labels(&palette).iter().all(|l| !l.contains("Quit")),
        "{palette:?}"
    );
    t.type_text(cx, "zzz");
    let palette = shown(cx, &t);
    assert!(palette.rows.is_empty());
    assert_eq!(palette.selected, None);
    assert_eq!(
        palette.footer.as_deref(),
        Some("No matching action available here")
    );

    t.press(cx, "escape");
    assert!(palette_closed(cx, &t));
    let focused = cx
        .update_window(t.window, |_, window, cx| {
            list_focus.is_focused(window) && window.focused(cx).is_some()
        })
        .unwrap();
    assert!(focused, "focus went back to the list");
    // The same key toggles it closed too.
    open_palette(cx, &t).await;
    t.press_key_of(cx, "palette::ToggleCommandPalette");
    assert!(palette_closed(cx, &t));
    assert!(list_focus_is_focused(cx, &t, &list_focus));
}

fn palette_closed(cx: &mut TestAppContext, t: &TestWorkspace) -> bool {
    palette(cx, t).is_none() && state(cx, t).0.is_none()
}

fn list_focus_is_focused(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    focus: &gpui_kit::FocusHandle,
) -> bool {
    cx.update_window(t.window, |_, window, _| focus.is_focused(window))
        .unwrap()
}

// covers UP-30: Enter runs the selected action from where focus was, as its key would: Focus
// Filter moves focus into the list's filter.
#[gpui_kit::test]
async fn enter_runs_the_selected_action(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    open_palette(cx, &t).await;
    t.type_text(cx, "focus filter");
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    assert_eq!(state(cx, &t).1, Focus::ListFilter);
}

// covers UP-30: the arrows and Ctrl-N / Ctrl-P move the selection, wrapping around; a click on
// a row runs it.
#[gpui_kit::test]
async fn keys_move_the_selection_and_a_click_runs_a_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    open_palette(cx, &t).await;
    let total = shown(cx, &t).total;
    t.press(cx, "down");
    assert_eq!(shown(cx, &t).selected, Some(1));
    t.press(cx, "ctrl-n");
    assert_eq!(shown(cx, &t).selected, Some(2));
    t.press(cx, "ctrl-p");
    t.press(cx, "up");
    assert_eq!(shown(cx, &t).selected, Some(0));
    t.press(cx, "up");
    assert_eq!(shown(cx, &t).selected, Some(total - 1));
    // Typing starts again from the best match.
    t.type_text(cx, "focus filter");
    assert_eq!(shown(cx, &t).selected, Some(0));
    assert_eq!(selected(&shown(cx, &t)), "Focus Filter");
    t.click(cx, ("palette-row", 0usize));
    assert!(palette_closed(cx, &t));
    assert_eq!(state(cx, &t).1, Focus::ListFilter);
}

// covers UP-30: only actions available now are listed (no Open Kind without a cluster, no
// list actions without a list, none of the palette's own), each with its category and its
// live shortcut as the OS writes it.
#[gpui_kit::test]
async fn lists_available_actions_with_categories_and_shortcuts(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_palette(cx, &t).await;
    let palette = shown(cx, &t);
    let all = labels(&palette);
    assert!(all.contains(&"Select Cluster…"), "{all:?}");
    assert!(all.contains(&"Quit Flint"), "{all:?}");
    assert!(all.contains(&"About Flint"), "{all:?}");
    assert!(
        !all.contains(&"Open Kind…"),
        "no cluster is active: {all:?}"
    );
    assert!(!all.contains(&"Focus Filter"), "no list is open: {all:?}");
    assert!(!all.contains(&"Show Command Palette"), "{all:?}");
    assert!(!all.contains(&"Close Command Palette"), "{all:?}");
    // The Edit actions are listed only where they act: a text field, or a shell tab. (Copy is
    // handled everywhere, by gpui-component's root, for text selected in the window.)
    for edit in ["Cut", "Copy", "Paste", "Select All"] {
        assert!(!all.contains(&edit), "{edit}: {all:?}");
    }
    assert_eq!(all.contains(&"Hide Flint"), cfg!(target_os = "macos"));
    let quit = palette
        .rows
        .iter()
        .find(|r| r.label == "Quit Flint")
        .unwrap();
    assert_eq!(quit.detail.as_deref(), Some("Flint"));
    assert_eq!(
        quit.shortcut.as_deref(),
        Some(registry::key_hint("secondary-q").as_str())
    );
    t.press(cx, "escape");

    // With a list open and focus in its filter, the list's actions and the Edit menu's appear.
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    t.press_key_of(cx, "resource_list::FocusFilter");
    assert_eq!(state(cx, &t).1, Focus::ListFilter);
    open_palette(cx, &t).await;
    let palette = shown(cx, &t);
    let all = labels(&palette);
    assert!(all.contains(&"Open Kind…"), "{all:?}");
    assert!(all.contains(&"Sort by Column…"), "{all:?}");
    assert!(all.contains(&"Copy"), "{all:?}");
    assert!(all.contains(&"Select All"), "{all:?}");
    assert!(!all.contains(&"Close Detail"), "no detail is open: {all:?}");
    let copy = palette.rows.iter().find(|r| r.label == "Copy").unwrap();
    assert_eq!(
        copy.shortcut.as_deref(),
        Some(registry::key_hint("secondary-c").as_str())
    );
    t.press(cx, "escape");

    // From the list's rows, Focus Filter shows its key; the Edit actions are gone.
    let list = t.list(cx).unwrap();
    let rows = list.read_with(cx, |l, _| l.focus_handle().clone());
    cx.update_window(t.window, |_, window, cx| window.focus(&rows, cx))
        .unwrap();
    open_palette(cx, &t).await;
    let palette = shown(cx, &t);
    assert!(!labels(&palette).contains(&"Copy"));
    let focus = palette
        .rows
        .iter()
        .find(|r| r.label == "Focus Filter")
        .unwrap();
    assert_eq!(focus.detail.as_deref(), Some("List"));
    assert_eq!(
        focus.shortcut.as_deref(),
        Some(registry::key_hint("secondary-f").as_str())
    );
}

// covers UP-30: an action that takes an argument asks for it in a second step, from a list:
// Select Cluster…, Open Kind… and Sort by Column… (reachable only from the palette, #29).
#[gpui_kit::test]
async fn actions_with_arguments_ask_for_them(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_palette(cx, &t).await;
    t.type_text(cx, "select cluster");
    assert_eq!(selected(&shown(cx, &t)), "Select Cluster…");
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert_eq!(palette.step.as_deref(), Some("Select Cluster…"));
    assert_eq!(palette.argument.as_deref(), Some("cluster"));
    assert_eq!(palette.query, "", "the field is cleared for the argument");
    assert_eq!(labels(&palette), ["alpha", "beta"]);
    t.type_text(cx, "bet");
    assert_eq!(labels(&shown(cx, &t)), ["beta"]);
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).shown_cluster() == Some("beta")
    })
    .await;

    // Open Kind… lists the kinds the cluster serves.
    t.connect_alpha(cx, crate::ui::support_tests::standard_sections())
        .await;
    open_palette(cx, &t).await;
    t.type_text(cx, "open kind");
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert_eq!(palette.step.as_deref(), Some("Open Kind…"));
    assert!(labels(&palette).contains(&"Pods"), "{palette:?}");
    t.type_text(cx, "pods");
    assert_eq!(selected(&shown(cx, &t)), "Pods");
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    t.feed_list(
        cx,
        crate::ui::support_tests::pods(&[("demo", "a", 3), ("demo", "b", 1)]),
    )
    .await;
    let workspace = t.workspace.clone();
    let kind = cx
        .update_window(t.window, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.view_state(window, cx).list.unwrap().kind)
        })
        .unwrap();
    assert_eq!(kind, "pods");

    // Sort by Column… lists the columns drawn.
    open_palette(cx, &t).await;
    t.type_text(cx, "sort by");
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert_eq!(palette.argument.as_deref(), Some("column"));
    assert_eq!(labels(&palette), ["Name", "Namespace", "Restarts"]);
    t.type_text(cx, "rest");
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    let workspace = t.workspace.clone();
    let sort = cx
        .update_window(t.window, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.view_state(window, cx).list.unwrap().sort)
        })
        .unwrap()
        .expect("sorted");
    assert_eq!(sort.column, "Restarts");
}

// covers UP-45 (#29): Open Logs by Label Selector… asks for a namespace (one may be typed that
// is not listed), then for the selector as text, and opens the tab in the dock.
#[gpui_kit::test]
async fn selector_logs_take_typed_arguments(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, crate::ui::support_tests::standard_sections())
        .await;
    open_palette(cx, &t).await;
    t.type_text(cx, "label selector");
    assert_eq!(selected(&shown(cx, &t)), "Open Logs by Label Selector…");
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert_eq!(palette.argument.as_deref(), Some("namespace"));
    t.type_text(cx, "demo");
    let palette = shown(cx, &t);
    let typed = &palette.rows[0];
    assert_eq!(typed.label, "“demo”");
    assert_eq!(typed.detail.as_deref(), Some("typed"));
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert_eq!(palette.argument.as_deref(), Some("selector"));
    assert_eq!(palette.action.as_deref(), Some("logs::OpenSelectorLogs"));
    // The heading keeps what was picked.
    assert_eq!(
        palette.step.as_deref(),
        Some("Open Logs by Label Selector… · demo")
    );
    // Enter with nothing typed says what to type, and keeps the step.
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert!(
        palette
            .error
            .as_deref()
            .is_some_and(|e| e.contains("label selector")),
        "{palette:?}"
    );
    assert_eq!(palette.argument.as_deref(), Some("selector"));
    t.type_text(cx, "app=web,tier!=db");
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    let dock = t.workspace.read_with(cx, |ws, _| ws.dock_view());
    assert_eq!(dock.read_with(cx, |d, _| d.tab_count()), 1);
}

// covers UP-30: a refused action keeps the palette open, saying why (the guard's reason).
#[gpui_kit::test]
async fn a_refusal_stays_in_the_palette(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_palette(cx, &t).await;
    t.type_text(cx, "hide cluster from ai");
    t.press(cx, "enter");
    t.type_text(cx, "alpha");
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert_eq!(
        palette.error.as_deref(),
        Some("The cluster is hidden already")
    );
    assert_eq!(palette.footer.as_deref(), palette.error.as_deref());
    assert_eq!(palette.step.as_deref(), Some("Hide Cluster from AI…"));
    // Typing again clears it.
    t.type_text(cx, "x");
    assert_eq!(shown(cx, &t).error, None);
}

// covers UP-30: recently run actions come first, latest first.
#[gpui_kit::test]
async fn recent_actions_come_first(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    for query in ["show all namespaces", "focus filter"] {
        // Back to the list, so both are available.
        let list = t.list(cx).unwrap();
        let focus = list.read_with(cx, |l, _| l.focus_handle().clone());
        cx.update_window(t.window, |_, window, cx| window.focus(&focus, cx))
            .unwrap();
        open_palette(cx, &t).await;
        t.type_text(cx, query);
        t.press(cx, "enter");
        assert!(palette_closed(cx, &t), "{query}");
    }
    open_palette(cx, &t).await;
    let palette = shown(cx, &t);
    assert_eq!(
        labels(&palette)[..2],
        ["Focus Filter", "Show All Namespaces"]
    );
    // With a query, a recent action that matches comes before the others that match.
    t.type_text(cx, "s");
    assert_eq!(selected(&shown(cx, &t)), "Show All Namespaces");
}

// About Flint (the app menu's first item, and in the palette) says the version, with only OK.
#[gpui_kit::test]
async fn about_flint_says_the_version(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.dispatch(cx, "flint::About", Value::Null).unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    let workspace = t.workspace.clone();
    let dialog = cx
        .update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            workspace.update(cx, |ws, cx| ws.view_state(window, cx).dialog)
        })
        .unwrap()
        .expect("the dialog is open");
    assert_eq!(dialog.title, "Flint");
    assert!(
        dialog.text.contains(env!("CARGO_PKG_VERSION")),
        "{}",
        dialog.text
    );
    assert_eq!(dialog.buttons, ["OK"]);
    t.dispatch(cx, "ui::Confirm", Value::Null).unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;
}

// The Edit menu's actions act on the focused text field, from the palette too: Select All then
// Cut empties the list's filter.
#[gpui_kit::test]
async fn edit_actions_reach_the_focused_field(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0), ("demo", "web", 0)])
        .await;
    t.press_key_of(cx, "resource_list::FocusFilter");
    t.type_text(cx, "web");
    let list = t.list(cx).unwrap();
    let filtered = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        filtered.read(cx).shown().1 == 1
    })
    .await;
    assert!(
        t.available(cx).contains(&"input::SelectAll"),
        "a focused field handles it"
    );
    open_palette(cx, &t).await;
    t.type_text(cx, "select all");
    assert_eq!(selected(&shown(cx, &t)), "Select All");
    t.press(cx, "enter");
    assert_eq!(state(cx, &t).1, Focus::ListFilter);
    t.dispatch(cx, "input::Cut", Value::Null).unwrap();
    cx.wait_for(t.window, WAIT, move |_, cx| list.read(cx).shown().1 == 2)
        .await;
    // Without a focused field, nothing handles them.
    t.focus_workspace(cx);
    assert_eq!(
        t.dispatch(cx, "input::Paste", Value::Null),
        Err(ActionError::NotAvailable("input::Paste".into()))
    );
}

// The palette is released once closed, its field included.
#[gpui_kit::test]
async fn a_closed_palette_is_released(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let palette = open_palette(cx, &t).await;
    let query = palette.read_with(cx, |p, _| p.query.clone());
    let (palette, query) = (weaken(palette), weaken(query));
    t.press(cx, "escape");
    assert!(palette_closed(cx, &t));
    cx.run_until_parked();
    palette.assert_released();
    query.assert_released();
}

#[test]
fn arguments_are_parsed_by_kind() {
    assert_eq!(
        arg_value(ArgKind::Text, " app=web ", true),
        Ok(json!("app=web"))
    );
    assert_eq!(arg_value(ArgKind::Number, "240", true), Ok(json!(240)));
    assert_eq!(arg_value(ArgKind::Number, "1.5", true), Ok(json!(1.5)));
    assert!(arg_value(ArgKind::Number, "wide", true).is_err());
    assert_eq!(arg_value(ArgKind::Bool, "Yes", true), Ok(json!(true)));
    assert_eq!(arg_value(ArgKind::Bool, "false", true), Ok(json!(false)));
    assert!(arg_value(ArgKind::Bool, "maybe", true).is_err());
    assert_eq!(
        arg_value(ArgKind::List, "a, b,,c", true),
        Ok(json!(["a", "b", "c"]))
    );
    assert!(arg_value(ArgKind::Namespace, "  ", true).is_err());
    // A value picked from a list is taken as it is: a context may be named "prod ".
    assert_eq!(
        arg_value(ArgKind::Cluster, "prod ", false),
        Ok(json!("prod "))
    );
    assert_eq!(arg_value(ArgKind::Text, " a ", false), Ok(json!(" a ")));
}

#[test]
fn refusals_read_as_sentences() {
    assert_eq!(
        refusal(&ActionError::Rejected {
            action: "workspace::OpenView".into(),
            reason: "no such kind".into()
        }),
        "No such kind"
    );
    assert_eq!(
        refusal(&ActionError::NotAvailable("detail::Close".into())),
        "Not available here any more"
    );
}

#[test]
fn choices_name_their_values() {
    let plain = Choice::plain("demo");
    assert_eq!(
        (plain.value.as_str(), plain.label.as_str()),
        ("demo", "demo")
    );
    assert_ne!(
        ArgInput::Text,
        ArgInput::Choices {
            items: vec![plain],
            free: true
        }
    );
}

#[test]
fn every_palette_action_is_bound() {
    crate::ui::registry::tests::assert_binds_scope(
        crate::ui::registry::Scope::Palette,
        CommandPalette::bind_actions,
    );
}

// The standard macOS menus (#29): every item a registered action, the palette in View.
#[gpui_kit::test]
async fn the_menus_hold_the_standard_items(cx: &mut TestAppContext) {
    use gpui_kit::OwnedMenuItem;
    let dir = tempfile::tempdir().unwrap();
    let _t = open_workspace(cx, dir.path()).await;
    let menus = cx.update(|cx| cx.get_menus()).unwrap();
    let items = |name: &str| -> Vec<String> {
        let menu = menus.iter().find(|m| m.name == name).unwrap();
        menu.items
            .iter()
            .filter_map(|item| match item {
                OwnedMenuItem::Action { name, action, .. } => {
                    assert!(registry::spec_of(&**action).is_some(), "{name}");
                    Some(name.to_string())
                }
                OwnedMenuItem::Submenu(menu) => Some(menu.name.to_string()),
                _ => None,
            })
            .collect()
    };
    let names: Vec<String> = menus.iter().map(|m| m.name.to_string()).collect();
    assert_eq!(names, ["Flint", "Edit", "View", "Window"]);
    assert_eq!(
        items("Flint"),
        [
            "About Flint",
            "Hide Flint",
            "Hide Others",
            "Show All",
            "Quit Flint"
        ]
    );
    assert_eq!(items("Edit"), ["Cut", "Copy", "Paste", "Select All"]);
    assert_eq!(
        items("View"),
        [
            "Show Command Palette",
            "Go to Resource",
            "Go Back",
            "Appearance"
        ]
    );
}

/// A pod `name` in `demo` with containers `app` and `sidecar`, selected and shown in the detail
/// pane.
async fn show_pod(cx: &mut TestAppContext, t: &TestWorkspace, name: &str) {
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": name}),
    )
    .unwrap();
    let detail = t.detail(cx).expect("the detail pane is open");
    let pod = flint_core::ObjectDetail {
        object: json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": name, "namespace": "demo", "uid": format!("uid-{name}")},
            "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]},
            "status": {"containerStatuses": [
                {"name": "app", "restartCount": 0},
                {"name": "sidecar", "restartCount": 0}
            ]}
        }),
        yaml: String::new(),
        managed_fields: None,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    };
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| d.show_fetched(pod, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
}

// covers UP-30: a container argument lists the shown pod's containers.
#[gpui_kit::test]
async fn the_container_step_lists_the_pods_containers(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    show_pod(cx, &t, "web").await;
    open_palette(cx, &t).await;
    t.type_text(cx, "open container logs");
    assert_eq!(selected(&shown(cx, &t)), "Open Container Logs…");
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert_eq!(palette.argument.as_deref(), Some("container"));
    assert_eq!(labels(&palette), ["app", "sidecar"]);
    t.type_text(cx, "side");
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    let dock = t.workspace.read_with(cx, |ws, _| ws.dock_view());
    let titles: Vec<String> = dock.read_with(cx, |dock, cx| {
        dock.tab_ids()
            .into_iter()
            .map(|id| dock.tab(id).unwrap().item(cx).tab_title().to_string())
            .collect()
    });
    assert_eq!(titles, ["web · sidecar"]);
}

// covers UP-30: Esc, or Backspace in the empty field, goes back a step and keeps the earlier
// picks; back at the actions, the query that found the action is there again.
#[gpui_kit::test]
async fn esc_and_backspace_go_back_a_step(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, crate::ui::support_tests::standard_sections())
        .await;
    open_palette(cx, &t).await;
    t.type_text(cx, "label selector");
    t.press(cx, "enter");
    t.type_text(cx, "demo");
    t.press(cx, "enter");
    assert_eq!(shown(cx, &t).argument.as_deref(), Some("selector"));
    // Esc: back to the namespace, the pick undone.
    t.press(cx, "escape");
    let palette = shown(cx, &t);
    assert_eq!(palette.argument.as_deref(), Some("namespace"));
    assert_eq!(
        palette.step.as_deref(),
        Some("Open Logs by Label Selector…")
    );
    assert_eq!(palette.query, "");
    // Forward again, then Backspace in the empty selector field goes back too.
    t.type_text(cx, "prod");
    t.press(cx, "enter");
    t.type_text(cx, "a");
    t.press(cx, "backspace");
    let palette = shown(cx, &t);
    assert_eq!(palette.query, "", "the first Backspace deletes, as usual");
    assert_eq!(palette.argument.as_deref(), Some("selector"));
    t.press(cx, "backspace");
    assert_eq!(shown(cx, &t).argument.as_deref(), Some("namespace"));
    t.press(cx, "backspace");
    let palette = shown(cx, &t);
    assert_eq!(palette.step, None, "back at the actions");
    assert_eq!(palette.query, "label selector");
    assert_eq!(selected(&palette), "Open Logs by Label Selector…");
    // At the actions, Esc closes.
    t.press(cx, "escape");
    assert!(palette_closed(cx, &t));
}

// covers UP-30: when the text typed is not one of the values listed, it comes first and is
// selected, so Enter on "kube" takes "kube", not "kube-system".
#[gpui_kit::test]
async fn typed_text_comes_before_listed_values(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, crate::ui::support_tests::standard_sections())
        .await;
    t.workspace.update(cx, |ws, _| {
        ws.set_namespaces_for_tests(vec!["demo".into(), "kube-system".into()])
    });
    open_palette(cx, &t).await;
    t.type_text(cx, "add namespace");
    t.press(cx, "enter");
    assert_eq!(labels(&shown(cx, &t)), ["demo", "kube-system"]);
    t.type_text(cx, "kube");
    let palette = shown(cx, &t);
    assert_eq!(labels(&palette), ["“kube”", "kube-system"]);
    assert_eq!(palette.selected, Some(0));
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    let filter = t
        .workspace
        .read_with(cx, |ws, _| ws.namespace_filter_of(&"alpha".into()));
    assert_eq!(filter, ["kube"]);
}

// covers UP-30: a pick list with nothing in it says so, and Enter does nothing.
#[gpui_kit::test]
async fn an_empty_pick_list_says_so(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    // Alpha never answers, so discovery never finishes: no kinds yet.
    t.activate(cx, "alpha").await;
    open_palette(cx, &t).await;
    t.type_text(cx, "open kind");
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert!(palette.rows.is_empty());
    assert_eq!(
        palette.footer.as_deref(),
        Some("No kinds (discovery has not finished)")
    );
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert_eq!(palette.error, None);
    assert_eq!(palette.argument.as_deref(), Some("kind"));
    // With some listed, a query that matches none says so.
    t.press(cx, "escape");
    t.press(cx, "escape");
    assert!(palette_closed(cx, &t));
    open_palette(cx, &t).await;
    t.type_text(cx, "select cluster");
    t.press(cx, "enter");
    t.type_text(cx, "zzz");
    assert_eq!(shown(cx, &t).footer.as_deref(), Some("Nothing matches"));
}

/// Makes the focus the open palette came from one that is no longer drawn, as when the view
/// that had it closes behind the palette.
fn strand_palette(cx: &mut TestAppContext, t: &TestWorkspace) {
    let palette = palette(cx, t).expect("the palette is open");
    palette.update(cx, |p, cx| p.previous = cx.focus_handle());
}

// covers UP-30: when the focus it came from is no longer drawn, the palette runs actions from
// the workspace instead: a workspace action and an App one both work.
#[gpui_kit::test]
async fn a_stale_focus_behind_the_palette_does_not_strand_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let (_, _shell) = crate::ui::support_tests::open_fake_shell(cx, &t);
    open_palette(cx, &t).await;
    strand_palette(cx, &t);

    // A workspace action.
    t.type_text(cx, "toggle dock");
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    let dock = t.workspace.read_with(cx, |ws, _| ws.dock_view());
    assert!(!dock.read_with(cx, |d, _| d.is_visible()), "the dock hid");
    assert_eq!(state(cx, &t).1, Focus::Workspace);

    // An App one: Quit asks first, since a shell runs.
    open_palette(cx, &t).await;
    strand_palette(cx, &t);
    t.type_text(cx, "quit flint");
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    t.dispatch(cx, "ui::Cancel", Value::Null).unwrap();
}

// An open context menu or drop-down is dismissed as the palette opens, so it neither stays
// drawn over the palette nor takes a click meant for it; focus goes back to where the menu
// came from, which is where the palette's actions run.
#[gpui_kit::test]
async fn opening_the_palette_dismisses_an_open_menu(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let (_, _shell) = crate::ui::support_tests::open_fake_shell(cx, &t);
    t.dispatch(cx, "dock::ShowTabList", Value::Null).unwrap();
    assert_eq!(state(cx, &t).1, Focus::Menu);
    open_palette(cx, &t).await;
    let workspace = t.workspace.clone();
    let menu = cx
        .update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            workspace.update(cx, |ws, cx| ws.view_state(window, cx).menu)
        })
        .unwrap();
    assert!(menu.is_none(), "{menu:?}");
    let dock = t.workspace.read_with(cx, |ws, _| ws.dock_view());
    assert!(dock.read_with(cx, |d, _| d.tab_menu_for_tests()).is_none());
    assert_eq!(state(cx, &t).1, Focus::Palette);
    // This menu keeps focus as it closes, so the palette runs from the workspace: nothing of
    // the closed menu's (its Confirm and Cancel) is listed.
    let all = labels(&shown(cx, &t))
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>();
    assert!(
        !all.iter().any(|l| l == "Confirm" || l == "Cancel"),
        "{all:?}"
    );
    assert!(all.iter().any(|l| l == "Toggle Dock"), "{all:?}");
    t.press(cx, "escape");
    assert!(palette_closed(cx, &t));
    assert_eq!(state(cx, &t).1, Focus::Workspace);
}

// covers UP-30: the window going to the background (Cmd-Tab) keeps the palette open, as a
// dialog stays; Esc then closes it and gives focus back.
#[gpui_kit::test]
async fn the_palette_stays_open_while_the_window_is_inactive(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    let list = t.list(cx).unwrap();
    let list_focus = list.read_with(cx, |l, _| l.focus_handle().clone());
    cx.update_window(t.window, |_, window, cx| window.focus(&list_focus, cx))
        .unwrap();
    let active = |cx: &mut TestAppContext| {
        cx.update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            window.is_window_active()
        })
        .unwrap()
    };
    cx.update_window(t.window, |_, window, _| window.activate_window())
        .unwrap();
    cx.run_until_parked();
    assert!(active(cx));
    open_palette(cx, &t).await;
    t.type_text(cx, "focus");
    gpui_kit::VisualTestContext::from_window(t.window, cx).deactivate_window();
    assert!(!active(cx), "the window went to the background");
    cx.run_until_parked();
    assert!(palette(cx, &t).is_some(), "still open in the background");
    cx.update_window(t.window, |_, window, _| window.activate_window())
        .unwrap();
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
    let (palette, focus) = state(cx, &t);
    assert_eq!(palette.expect("still open").query, "focus");
    assert_eq!(focus, Focus::Palette);
    t.press(cx, "escape");
    assert!(palette_closed(cx, &t));
    assert!(list_focus_is_focused(cx, &t, &list_focus));
}

// Edit > Copy and Paste act in a shell tab (its own copy and paste), and the palette lists
// them there; Cut and Select All are not.
#[gpui_kit::test]
async fn edit_copy_and_paste_work_in_a_shell_tab(cx: &mut TestAppContext) {
    use gpui_kit::ClipboardItem;
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let (_, mut shell) = crate::ui::support_tests::open_fake_shell(cx, &t);
    assert_eq!(state(cx, &t).1, Focus::Shell);
    open_palette(cx, &t).await;
    let all = labels(&shown(cx, &t))
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>();
    assert!(all.iter().any(|l| l == "Copy"), "{all:?}");
    assert!(all.iter().any(|l| l == "Paste"), "{all:?}");
    assert!(
        !all.iter().any(|l| l == "Cut" || l == "Select All"),
        "{all:?}"
    );
    // Not a second Cmd-C next to Copy Terminal Selection: Copy's key is a text field's.
    let copy = shown(cx, &t)
        .rows
        .into_iter()
        .find(|r| r.label == "Copy")
        .unwrap();
    assert_eq!(copy.shortcut, None);
    t.press(cx, "escape");

    cx.write_to_clipboard(ClipboardItem::new_string("echo one".into()));
    shell.clear_written();
    t.dispatch(cx, "input::Paste", Value::Null).unwrap();
    shell.pump();
    assert_eq!(shell.written(), b"echo one");
}

/// Opens the palette's argument step of the action found by `query`.
async fn ask(cx: &mut TestAppContext, t: &TestWorkspace, query: &str) -> PaletteState {
    open_palette(cx, t).await;
    t.type_text(cx, query);
    t.press(cx, "enter");
    let palette = shown(cx, t);
    assert!(palette.argument.is_some(), "{query}: {palette:?}");
    palette
}

// A listed value equal to the text typed, ignoring case, comes first and is selected.
#[gpui_kit::test]
async fn an_exact_match_comes_first(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, crate::ui::support_tests::standard_sections())
        .await;
    t.workspace.update(cx, |ws, _| {
        ws.set_namespaces_for_tests(vec!["a-bc".into(), "abc".into()])
    });
    ask(cx, &t, "add namespace").await;
    t.type_text(cx, "ABC");
    let palette = shown(cx, &t);
    assert_eq!(
        labels(&palette),
        ["abc", "a-bc"],
        "no typed row: it is listed"
    );
    assert_eq!(palette.selected, Some(0));
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    let filter = t
        .workspace
        .read_with(cx, |ws, _| ws.namespace_filter_of(&"alpha".into()));
    assert_eq!(filter, ["abc"]);
}

// A click on an action that takes arguments leaves focus in the field for the next step.
#[gpui_kit::test]
async fn a_click_keeps_typing_in_the_field(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_palette(cx, &t).await;
    t.type_text(cx, "select cluster");
    assert_eq!(selected(&shown(cx, &t)), "Select Cluster…");
    t.click(cx, ("palette-row", 0usize));
    let (palette, focus) = state(cx, &t);
    assert_eq!(palette.unwrap().argument.as_deref(), Some("cluster"));
    assert_eq!(focus, Focus::Palette);
    t.type_text(cx, "bet");
    assert_eq!(labels(&shown(cx, &t)), ["beta"]);
}

// A refusal at the last step keeps the earlier picks, so Enter again retries with them.
#[gpui_kit::test]
async fn a_refusal_keeps_the_earlier_picks(cx: &mut TestAppContext) {
    use flint_core::{ConnectionState, Event, ServerVersion};
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, crate::ui::support_tests::standard_sections())
        .await;
    ask(cx, &t, "label selector").await;
    t.type_text(cx, "demo");
    t.press(cx, "enter");
    t.type_text(cx, "app=web");
    // Alpha disconnects before Enter: logs cannot open there now.
    t.feed(
        cx,
        Event::ConnectionChanged {
            cluster: "alpha".into(),
            state: ConnectionState::Disconnected,
        },
    );
    t.press(cx, "enter");
    let palette = shown(cx, &t);
    assert_eq!(
        palette.error.as_deref(),
        Some("Not available here any more")
    );
    assert_eq!(
        palette.step.as_deref(),
        Some("Open Logs by Label Selector… · demo")
    );
    t.feed(
        cx,
        Event::ConnectionChanged {
            cluster: "alpha".into(),
            state: ConnectionState::Connected(ServerVersion {
                git_version: "v1.33.1".into(),
                platform: "linux/arm64".into(),
            }),
        },
    );
    t.type_text(cx, "");
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    let dock = t.workspace.read_with(cx, |ws, _| ws.dock_view());
    assert_eq!(dock.read_with(cx, |d, _| d.tab_count()), 1);
}

/// Shows pod `web` with an init container, `app` running and `sidecar` waiting.
async fn show_pod_with_init(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    let detail = t.detail(cx).expect("the detail pane is open");
    let pod = flint_core::ObjectDetail {
        object: json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web"},
            "spec": {
                "initContainers": [{"name": "setup"}],
                "containers": [{"name": "app"}, {"name": "sidecar"}]
            },
            "status": {
                "initContainerStatuses": [
                    {"name": "setup", "restartCount": 0, "state": {"terminated": {"exitCode": 0}}}
                ],
                "containerStatuses": [
                    {"name": "app", "restartCount": 0, "state": {"running": {}}},
                    {"name": "sidecar", "restartCount": 2,
                     "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]
            }
        }),
        yaml: String::new(),
        managed_fields: None,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    };
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| d.show_fetched(pod, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
}

// Open Container Shell… offers only the running app containers a shell can open in; Open
// Container Logs… every container, init ones included.
#[gpui_kit::test]
async fn shells_offer_only_running_app_containers(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    show_pod_with_init(cx, &t).await;
    let palette = ask(cx, &t, "open container shell").await;
    assert_eq!(labels(&palette), ["app"]);
    assert_eq!(palette.rows[0].detail.as_deref(), Some("default"));
    t.press(cx, "escape");
    t.press(cx, "escape");
    let palette = ask(cx, &t, "open container logs").await;
    let all = labels(&palette);
    assert!(
        all.contains(&"setup") && all.contains(&"sidecar"),
        "{all:?}"
    );
}

// A value picked from the list is used as it is, spaces included.
#[gpui_kit::test]
async fn a_picked_value_is_not_trimmed(cx: &mut TestAppContext) {
    use crate::ui::support_tests::{kubeconfig_yaml, open_workspace_with};
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, servers| {
        let kubeconfig = dir.join("kubeconfig");
        std::fs::write(
            &kubeconfig,
            kubeconfig_yaml(&[("prod ", &servers.silent), ("prod", &servers.refused)]),
        )
        .unwrap();
        vec![kubeconfig]
    })
    .await;
    ask(cx, &t, "select cluster").await;
    let palette = shown(cx, &t);
    let ix = palette
        .rows
        .iter()
        .position(|r| r.label == "prod " || r.detail.as_deref() == Some("prod "))
        .expect("prod  is listed");
    t.click(cx, ("palette-row", ix));
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).shown_cluster() == Some("prod ")
    })
    .await;
}

/// Opens pods `pod-00` to `pod-29`, so list rows lie under the palette and below it.
async fn open_many_pods(cx: &mut TestAppContext, t: &TestWorkspace) {
    let names: Vec<String> = (0..30).map(|i| format!("pod-{i:02}")).collect();
    let rows: Vec<(&str, &str, i64)> = names.iter().map(|n| ("demo", n.as_str(), 0)).collect();
    t.open_pods(cx, &rows).await;
}

// The panel is as tall as its content, and pointer events stop at it: a click on it reaches
// no row behind it and keeps focus in the field; a click on a row below it is outside, and
// closes it, also while it asks for an argument.
#[gpui_kit::test]
async fn the_panel_takes_its_own_clicks_only(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    open_many_pods(cx, &t).await;
    open_palette(cx, &t).await;
    let panel = t.find(cx, "command-palette").expect("drawn").bounds();
    let height = cx
        .update_window(t.window, |_, window, _| window.viewport_size().height)
        .unwrap();
    assert!(
        panel.bottom() < height - gpui_kit::px(100.),
        "{panel:?} in a window {height:?} tall"
    );
    // The footer lies over the list's rows.
    t.click(cx, "palette-footer");
    assert_eq!(t.selected(cx), None, "the click stayed in the panel");
    assert!(palette(cx, &t).is_some());

    // During an argument step, a click on a row below the panel closes it, rather than going
    // back a step.
    assert_eq!(state(cx, &t).1, Focus::Palette);
    t.type_text(cx, "select cluster");
    t.press(cx, "enter");
    assert!(shown(cx, &t).argument.is_some());
    let below = t.find(cx, "row-uid-pod-25").expect("drawn").bounds();
    assert!(below.top() > panel.bottom(), "{below:?} under {panel:?}");
    t.click(cx, "row-uid-pod-25");
    assert!(palette_closed(cx, &t));
}

// The shortcut shown is the one that fires from where the palette came from: in a log tab,
// Cmd-R is Refresh Object while the tab has nothing to reconnect.
#[gpui_kit::test]
async fn shortcuts_are_the_ones_that_fire_there(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    show_pod(cx, &t, "web").await;
    t.dispatch(cx, "detail::OpenLogs", Value::Null).unwrap();
    assert_eq!(state(cx, &t).1, Focus::Log);
    open_palette(cx, &t).await;
    let palette = shown(cx, &t);
    let refresh = palette
        .rows
        .iter()
        .find(|r| r.label == "Refresh Object")
        .expect("listed");
    assert_eq!(
        refresh.shortcut.as_deref(),
        Some(registry::key_hint("secondary-r").as_str())
    );
    assert!(!labels(&palette).contains(&"Reconnect Logs"));
}

// covers UP-30: an exact stored value wins over what is shown for it, case first. With
// contexts `cluster/prod` and an EKS ARN shown as `cluster/prod`, typing the name picks that
// context; contexts `ABC` and `abc` each get their own.
#[gpui_kit::test]
async fn exact_stored_values_win(cx: &mut TestAppContext) {
    use crate::ui::support_tests::{kubeconfig_yaml, open_workspace_with};
    const ARN: &str = "arn:aws:eks:eu-west-1:123456789012:cluster/prod";
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace_with(cx, dir.path(), |dir, servers| {
        let kubeconfig = dir.join("kubeconfig");
        std::fs::write(
            &kubeconfig,
            kubeconfig_yaml(&[
                (ARN, &servers.refused),
                ("cluster/prod", &servers.refused),
                ("ABC", &servers.refused),
                ("abc", &servers.refused),
            ]),
        )
        .unwrap();
        vec![kubeconfig]
    })
    .await;
    for (typed, wanted) in [
        ("cluster/prod", "cluster/prod"),
        ("abc", "abc"),
        ("ABC", "ABC"),
    ] {
        ask(cx, &t, "select cluster").await;
        t.type_text(cx, typed);
        t.press(cx, "enter");
        assert!(palette_closed(cx, &t), "{typed}");
        let workspace = t.workspace.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| {
            workspace.read(cx).shown_cluster() == Some(wanted)
        })
        .await;
    }
}

// covers UP-30: a context removed and added back at the same URL is a new slot, so the switcher
// does not restore the old selection onto it.
#[test]
fn a_selection_stays_only_on_the_same_slot() {
    use super::same_slot;
    let id = Some(("a".to_owned(), Some("https://example.invalid".to_owned())));
    let other = Some(("b".to_owned(), Some("https://example.invalid".to_owned())));
    assert!(same_slot((&id, Some(1)), (&id, Some(1))));
    assert!(!same_slot((&id, Some(1)), (&id, Some(2))), "started over");
    assert!(!same_slot((&id, Some(1)), (&other, Some(1))));
}

#[test]
fn exactness_ranks_stored_values_then_labels_case_first() {
    use super::exactness;
    let arn = Choice {
        value: "arn:aws:eks:eu-west-1:1:cluster/prod".into(),
        label: "cluster/prod".into(),
        detail: None,
    };
    let plain = Choice::plain("cluster/prod");
    let rank = |typed: &str, choice: &Choice| exactness(typed, &typed.to_lowercase(), choice);
    assert!(rank("cluster/prod", &plain) < rank("cluster/prod", &arn));
    assert!(rank("abc", &Choice::plain("abc")) < rank("abc", &Choice::plain("ABC")));
    assert!(rank("abc", &Choice::plain("ABC")) < rank("abc", &Choice::plain("abcd")));
}

// covers UP-30: an open step's choices follow the data: Open Kind… opened before discovery
// finishes lists the kinds once it does, for the query typed meanwhile.
#[gpui_kit::test]
async fn an_open_step_follows_its_data(cx: &mut TestAppContext) {
    use flint_core::Event;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.activate(cx, "alpha").await;
    let palette = ask(cx, &t, "open kind").await;
    assert_eq!(
        palette.footer.as_deref(),
        Some("No kinds (discovery has not finished)")
    );
    t.type_text(cx, "pod");
    t.feed(
        cx,
        Event::DiscoveryChanged {
            cluster: "alpha".into(),
            generation: crate::ui::support_tests::CURRENT_GENERATION,
            discovery: Some(Arc::new(crate::ui::support_tests::discovery(
                crate::ui::support_tests::standard_sections(),
            ))),
            error: None,
        },
    );
    let palette = shown(cx, &t);
    assert_eq!(palette.query, "pod");
    assert_eq!(palette.argument.as_deref(), Some("kind"));
    assert_eq!(selected(&palette), "Pods");
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
}

// covers UP-30: a refreshed list keeps the selected value selected.
#[gpui_kit::test]
async fn a_refreshed_step_keeps_the_selection(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, crate::ui::support_tests::standard_sections())
        .await;
    t.workspace.update(cx, |ws, _| {
        ws.set_namespaces_for_tests(vec!["demo".into(), "kube-system".into()])
    });
    ask(cx, &t, "add namespace").await;
    t.press(cx, "down");
    assert_eq!(selected(&shown(cx, &t)), "kube-system");
    // The namespace watch reports a new one, sorted before the selected.
    t.workspace.update(cx, |ws, cx| {
        ws.set_namespaces_for_tests(vec!["demo".into(), "default".into(), "kube-system".into()]);
        cx.notify();
    });
    cx.run_until_parked();
    let palette = shown(cx, &t);
    assert_eq!(labels(&palette), ["demo", "default", "kube-system"]);
    assert_eq!(selected(&palette), "kube-system");
}

// Opening the palette closes an open popover (the namespace filter), which is drawn above
// everything else; the palette then has focus.
#[gpui_kit::test]
async fn opening_the_palette_dismisses_a_popover(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    t.click(cx, "namespace-filter-button");
    t.wait_for_element(cx, "ns-all").await;
    t.click(cx, "ns-add");
    assert_eq!(state(cx, &t).1, Focus::NamespaceInput);
    open_palette(cx, &t).await;
    assert!(t.find(cx, "ns-all").is_none(), "the popover closed");
    assert_eq!(state(cx, &t).1, Focus::Palette);
}

// A menu that gives focus back as it closes (a row's context menu gives it to the list): the
// palette runs from there, and Esc returns focus to the list.
#[gpui_kit::test]
async fn a_menu_that_gives_focus_back_is_where_the_palette_runs(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    let row = gpui_kit::SharedString::from("row-uid-web");
    cx.update_window(t.window, |_, window, cx| window.right_click(row, cx))
        .unwrap();
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("popup-menu").is_some()
    })
    .await;
    assert_eq!(state(cx, &t).1, Focus::Menu);
    open_palette(cx, &t).await;
    assert!(t.find(cx, "popup-menu").is_none(), "the row's menu closed");
    // From the list: its actions are listed.
    assert!(labels(&shown(cx, &t)).contains(&"Focus Filter"));
    t.press(cx, "escape");
    assert!(palette_closed(cx, &t));
    assert_eq!(state(cx, &t).1, Focus::List);
}

// A refresh does not keep a selection nobody chose: an exact match that arrives becomes the
// selected row, so Enter on "prods" with `prods-eu` listed first picks `prods` once it appears.
#[gpui_kit::test]
async fn a_refresh_selects_an_exact_match_that_arrives(cx: &mut TestAppContext) {
    use crate::ui::support_tests::{discovery, entry, section};
    use flint_core::{Event, SectionKind};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let eu = entry("example.com", "prods-eu", "ProdEu", "Prods Eu", true);
    t.connect_alpha(
        cx,
        vec![section("Custom", SectionKind::BuiltIn, vec![eu.clone()])],
    )
    .await;
    ask(cx, &t, "open kind").await;
    t.type_text(cx, "prods");
    assert_eq!(selected(&shown(cx, &t)), "Prods Eu");
    let prods = entry("", "prods", "Prod", "Prods", true);
    t.feed(
        cx,
        Event::DiscoveryChanged {
            cluster: "alpha".into(),
            generation: crate::ui::support_tests::CURRENT_GENERATION,
            discovery: Some(Arc::new(discovery(vec![section(
                "Custom",
                SectionKind::BuiltIn,
                vec![eu, prods],
            )]))),
            error: None,
        },
    );
    let palette = shown(cx, &t);
    assert_eq!(labels(&palette), ["Prods", "Prods Eu"]);
    assert_eq!(selected(&palette), "Prods");
    t.press(cx, "enter");
    assert!(palette_closed(cx, &t));
    let workspace = t.workspace.clone();
    let kind = cx
        .update_window(t.window, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.view_state(window, cx).list.map(|l| l.kind))
        })
        .unwrap();
    assert_eq!(kind.as_deref(), Some("prods"));
}

// A click drawn on rows that a live refresh has since reordered is dropped rather than pick
// the entry now in that row; a click on the rows shown runs.
#[gpui_kit::test]
async fn a_click_on_replaced_rows_is_dropped(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, crate::ui::support_tests::standard_sections())
        .await;
    t.workspace.update(cx, |ws, _| {
        ws.set_namespaces_for_tests(vec!["demo".into(), "kube-system".into()])
    });
    ask(cx, &t, "add namespace").await;
    assert_eq!(labels(&shown(cx, &t)), ["demo", "kube-system"]);
    let drawn = palette(cx, &t)
        .unwrap()
        .read_with(cx, |p, _| p.rows_version);
    // A namespace appears above the one about to be clicked.
    t.workspace.update(cx, |ws, cx| {
        ws.set_namespaces_for_tests(vec!["alpha".into(), "demo".into(), "kube-system".into()]);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(labels(&shown(cx, &t)), ["alpha", "demo", "kube-system"]);
    // The click on `kube-system` (row 1 when drawn) arrives now: dropped.
    t.dispatch(cx, "palette::PalettePick", json!({"row": 1, "rows": drawn}))
        .unwrap();
    assert!(palette(cx, &t).is_some(), "nothing ran");
    let filter = |cx: &mut TestAppContext| {
        t.workspace
            .read_with(cx, |ws, _| ws.namespace_filter_of(&"alpha".into()))
    };
    assert!(filter(cx).is_empty());
    // On the rows shown, row 2 is `kube-system`.
    t.click(cx, ("palette-row", 2usize));
    assert!(palette_closed(cx, &t));
    assert_eq!(filter(cx), ["kube-system"]);
}

// The namespace filter's popover closes when the palette opens also when focus has moved off
// it (Focus Filter), since it is closed by its open state, not through focus.
#[gpui_kit::test]
async fn a_popover_without_focus_closes_when_the_palette_opens(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    t.click(cx, "namespace-filter-button");
    t.wait_for_element(cx, "ns-all").await;
    t.press_key_of(cx, "resource_list::FocusFilter");
    assert_eq!(state(cx, &t).1, Focus::ListFilter);
    assert!(t.find(cx, "ns-all").is_some(), "the popover stays open");
    open_palette(cx, &t).await;
    assert!(t.find(cx, "ns-all").is_none(), "the popover closed");
    assert_eq!(state(cx, &t).1, Focus::Palette);
    // It opens again from its button once the palette is gone.
    t.press(cx, "escape");
    t.click(cx, "namespace-filter-button");
    t.wait_for_element(cx, "ns-all").await;
}
