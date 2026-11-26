//! A cluster's colour tag (ADR-0014): the dot in its sidebar row, the strip over the detail
//! pane, and the row menu's submenu that sets or clears it. A tag is cosmetic and always has a
//! name for assistive technology, so colour is never the only signal.

use flint_core::settings::{ClusterTag, server_identity};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Root, Theme, h_flex};
use gpui_kit::*;

use super::menu_fit::wrapped_label;
use super::registry::{self, ClearClusterTag, SetClusterTag};
use super::theme::cluster_tag_color;
use super::workspace::Workspace;

/// How a tag is named on screen and for assistive technology.
pub fn tag_text(tag: ClusterTag) -> String {
    format!("Colour tag: {}", tag.label())
}

/// The dot in a cluster's sidebar row.
pub fn tag_dot(cluster: &str, tag: ClusterTag, theme: &Theme) -> impl IntoElement {
    let text = SharedString::from(tag_text(tag));
    let tip = text.clone();
    div()
        .id(SharedString::from(format!("cluster-tag-{cluster}")))
        .flex_none()
        .size_2()
        .rounded_full()
        .bg(cluster_tag_color(tag, theme))
        .role(Role::Image)
        .aria_label(text)
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .test_support()
}

/// The strip along the top of the detail pane. Put it in a `relative()` pane: it takes no room.
pub fn tag_strip(tag: ClusterTag, theme: &Theme) -> impl IntoElement {
    div()
        .id("detail-cluster-tag")
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(3.))
        .bg(cluster_tag_color(tag, theme))
        .role(Role::Image)
        .aria_label(tag_text(tag))
        .test_support()
}

/// Adds the Colour Tag submenu to a cluster row's menu. `endpoint` is the row's
/// [`flint_core::ClusterSummary::endpoint`]: a choice made after the context moved is refused,
/// so a colour picked for one server is never saved for another.
pub fn tag_submenu(
    menu: PopupMenu,
    cluster: String,
    endpoint: u64,
    current: Option<ClusterTag>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let server = shown_server(&cluster, window, cx);
    menu.submenu("Colour Tag", window, cx, move |sub, _, _| {
        let sub = ClusterTag::ALL.into_iter().fold(sub, |sub, tag| {
            sub.item(
                PopupMenuItem::element(move |_, cx| tag_choice(tag, cx.theme()))
                    .checked(current == Some(tag))
                    .on_click(while_showing(
                        cluster.clone(),
                        server.clone(),
                        registry::on_click(SetClusterTag {
                            cluster: cluster.clone(),
                            tag,
                            endpoint: Some(endpoint),
                        }),
                    )),
            )
        });
        sub.separator().item(
            PopupMenuItem::new("No Tag")
                .disabled(current.is_none())
                .on_click(while_showing(
                    cluster.clone(),
                    server.clone(),
                    registry::on_click(ClearClusterTag {
                        cluster: cluster.clone(),
                        endpoint: Some(endpoint),
                    }),
                )),
        )
    })
}

/// A colour choice's content. Its label's value names the menu item around it.
fn tag_choice(tag: ClusterTag, theme: &Theme) -> impl IntoElement + use<> {
    let label = SharedString::from(tag.label());
    h_flex()
        .id(SharedString::from(format!("menu-cluster-tag-{label}")))
        .gap_2()
        .items_center()
        .child(
            div()
                .size_2()
                .rounded_full()
                .bg(cluster_tag_color(tag, theme)),
        )
        .child(choice_label(tag))
        .test_support()
}

fn choice_label(tag: ClusterTag) -> impl IntoElement {
    wrapped_label(tag.label().into())
}

/// The server `window`'s sidebar shows for `cluster` now.
fn shown_server(cluster: &str, window: &Window, cx: &App) -> Option<String> {
    let root = window.root::<Root>().flatten()?;
    let workspace = root.read(cx).view().clone().downcast::<Workspace>().ok()?;
    workspace.read(cx).tag_menu_server(cluster, cx)
}

/// `perform`, unless `cluster` no longer shows `server`. A file that becomes unreadable can show
/// a later file's server without changing the endpoint revision the action compares. URLs
/// compare as tag lookup does, without a trailing slash.
fn while_showing<E: ?Sized>(
    cluster: String,
    server: Option<String>,
    perform: impl Fn(&E, &mut Window, &mut App) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    move |event, window, cx| {
        let shown = shown_server(&cluster, window, cx);
        let same = matches!(
            (shown.as_deref(), server.as_deref()),
            (Some(a), Some(b)) if server_identity(a) == server_identity(b)
        );
        if same {
            perform(event, window, cx);
        } else {
            // Names no URL: a server URL may hold credentials.
            tracing::warn!("{cluster} now shows a different server; colour tag not changed");
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::accesskit::{Node, Role};
    use gpui_kit::component::ActiveTheme as _;
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{AppContext as _, Element as _, IntoElement, TestAppContext};
    use serde_json::json;

    use super::{
        ClusterTag, choice_label, shown_server, tag_choice, tag_dot, tag_strip, tag_text,
        while_showing,
    };
    use crate::ui::palette::ArgInput;
    use crate::ui::registry::{self, ClearClusterTag, SetClusterTag, spec_of};
    use crate::ui::support_tests::{TestWorkspace, WAIT, alpha_and_beta, open_workspace_with};

    /// The node GPUI adds to the accessibility tree for `element`, if any. The test platform
    /// never activates the tree, so this reads what the element writes to it.
    fn node_of(element: impl IntoElement) -> Option<Node> {
        let element = element.into_element();
        let mut node = Node::new(element.a11y_role()?);
        element.write_a11y_info(&mut node);
        Some(node)
    }

    // covers UP-83: every colour has its own name, so the tag never relies on colour alone.
    #[test]
    fn every_tag_has_its_own_accessible_name() {
        let mut names: Vec<String> = ClusterTag::ALL.into_iter().map(tag_text).collect();
        assert_eq!(names[5], "Colour tag: Red");
        names.sort();
        names.dedup();
        assert_eq!(names.len(), ClusterTag::ALL.len());
    }

    // covers UP-83: the dot and the strip are named nodes, not only test metadata.
    #[gpui_kit::test]
    fn the_dot_and_the_strip_are_accessibility_nodes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            let theme = cx.theme().clone();
            for node in [
                node_of(tag_dot("ctx", ClusterTag::Red, &theme)),
                node_of(tag_strip(ClusterTag::Red, &theme)),
            ] {
                let node = node.expect("a node in the accessibility tree");
                assert_eq!(node.role(), Role::Image);
                assert_eq!(node.label(), Some("Colour tag: Red"));
            }
        });
    }

    // covers UP-83: a colour's text names the actionable menu item around it, which carries the
    // checked state. The content adds no second, inert menu item.
    #[gpui_kit::test]
    fn a_colour_choice_names_its_menu_item(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            let theme = cx.theme().clone();
            assert!(node_of(tag_choice(ClusterTag::Teal, &theme)).is_none());
            let label = node_of(choice_label(ClusterTag::Teal)).expect("a label node");
            assert_eq!(label.role(), Role::Label);
            assert_eq!(label.value(), Some("Teal"));
        });
    }

    // covers UP-83: the action goes through the core to `settings.toml`, the sidebar shows the
    // named dot, and clearing removes it. A menu drawn for an older endpoint changes nothing.
    #[gpui_kit::test]
    async fn a_tag_is_set_shown_and_cleared(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace_with(cx, dir.path(), alpha_and_beta).await;
        let tag_of = |model: &gpui_kit::Entity<crate::ui::clusters::ClustersModel>,
                      cx: &gpui_kit::App| {
            let model = model.read(cx);
            let cluster = model.clusters.iter().find(|c| c.info.name == "kind-alpha");
            cluster.and_then(|c| model.cluster_tag(c))
        };

        let stale = t.model.read_with(cx, |m, _| m.clusters[0].endpoint) + 1;
        let refused = t.dispatch(
            cx,
            "workspace::SetClusterTag",
            json!({"cluster": "kind-alpha", "tag": "red", "endpoint": stale}),
        );
        assert!(
            refused.is_err(),
            "the context moved since the menu was drawn"
        );

        t.dispatch(
            cx,
            "workspace::SetClusterTag",
            json!({"cluster": "kind-alpha", "tag": "red"}),
        )
        .unwrap();
        let model = t.model.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| {
            tag_of(&model, cx) == Some(ClusterTag::Red)
        })
        .await;
        cx.run_until_parked();
        cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        // The sidebar's dot; the home screen's row draws one too.
        let label = cx
            .update_window(t.window, |_, window, _| {
                window
                    .within("cluster-list")
                    .find("cluster-tag-kind-alpha")
                    .label()
                    .map(str::to_owned)
            })
            .unwrap();
        assert_eq!(label.as_deref(), Some("Colour tag: Red"));
        let text = std::fs::read_to_string(dir.path().join("settings.toml")).unwrap();
        assert!(text.contains("cluster_tags"), "{text}");

        t.dispatch(
            cx,
            "workspace::ClearClusterTag",
            json!({"cluster": "kind-alpha"}),
        )
        .unwrap();
        let model = t.model.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| tag_of(&model, cx).is_none())
            .await;
        cx.run_until_parked();
        cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        assert!(t.find(cx, "cluster-tag-kind-alpha").is_none());
    }

    // covers UP-83: a menu left open while the sidebar falls back to another file's server, with
    // the endpoint revision unchanged, saves nothing there. A trailing slash is the same server.
    #[gpui_kit::test]
    async fn a_menu_left_open_across_a_fallback_changes_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace_with(cx, dir.path(), alpha_and_beta).await;
        let index = t.model.read_with(cx, |m, _| {
            m.clusters
                .iter()
                .position(|c| c.info.name == "kind-alpha")
                .unwrap()
        });
        let endpoint = t.model.read_with(cx, |m, _| m.clusters[index].endpoint);
        let set = || {
            registry::on_click::<(), _>(SetClusterTag {
                cluster: "kind-alpha".into(),
                tag: ClusterTag::Red,
                endpoint: Some(endpoint),
            })
        };
        let captured = cx
            .update_window(t.window, |_, window, cx| {
                shown_server("kind-alpha", window, cx)
            })
            .unwrap();
        assert_eq!(captured.as_deref(), Some(t.server.as_str()));
        let stale = while_showing("kind-alpha".into(), captured, set());

        let fallback = t.refused_server.clone();
        t.model.update(cx, |m, _| {
            m.clusters[index].info.server = Some(fallback.clone());
        });
        cx.update_window(t.window, |_, window, cx| stale(&(), window, cx))
            .unwrap();
        // The core takes commands in order: once this one is back, the click's would be too.
        t.dispatch(
            cx,
            "workspace::SetClusterTag",
            json!({"cluster": "kind-beta", "tag": "blue"}),
        )
        .unwrap();
        let model = t.model.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| {
            let model = model.read(cx);
            model
                .clusters
                .iter()
                .find(|c| c.info.name == "kind-beta")
                .is_some_and(|c| model.cluster_tag(c).is_some())
        })
        .await;
        let text = std::fs::read_to_string(dir.path().join("settings.toml")).unwrap();
        assert!(!text.contains("kind-alpha"), "{text}");

        let shown = t.server.clone();
        t.model.update(cx, |m, _| {
            m.clusters[index].info.server = Some(shown.clone());
        });
        let slashed = while_showing("kind-alpha".into(), Some(format!("{shown}/")), set());
        cx.update_window(t.window, |_, window, cx| slashed(&(), window, cx))
            .unwrap();
        let model = t.model.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| {
            let model = model.read(cx);
            model.cluster_tag(&model.clusters[index]) == Some(ClusterTag::Red)
        })
        .await;
    }

    // covers UP-83: the palette offers to clear only a cluster that has a tag.
    #[gpui_kit::test]
    async fn the_palette_clears_only_tagged_clusters(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace_with(cx, dir.path(), alpha_and_beta).await;
        let spec = spec_of(&ClearClusterTag::default()).unwrap();
        let offered = |t: &TestWorkspace, cx: &mut TestAppContext| {
            t.workspace.read_with(cx, |ws, cx| {
                match ws.palette_choices(spec, &spec.args[0], cx) {
                    ArgInput::Choices { items, .. } => {
                        items.into_iter().map(|c| c.value).collect::<Vec<_>>()
                    }
                    ArgInput::Text => panic!("a cluster is picked from a list"),
                }
            })
        };
        assert!(offered(&t, cx).is_empty());

        t.dispatch(
            cx,
            "workspace::SetClusterTag",
            json!({"cluster": "kind-alpha", "tag": "red"}),
        )
        .unwrap();
        let model = t.model.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| {
            let model = model.read(cx);
            model
                .clusters
                .iter()
                .any(|c| model.cluster_tag(c).is_some())
        })
        .await;
        assert_eq!(offered(&t, cx), ["kind-alpha"]);
    }
}
