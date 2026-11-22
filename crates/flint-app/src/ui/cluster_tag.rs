//! A cluster's colour tag (ADR-0014): the dot in its sidebar row, the strip over the detail
//! pane, and the row menu's submenu that sets or clears it. A tag is cosmetic and always has a
//! name for assistive technology, so colour is never the only signal.

use flint_core::settings::ClusterTag;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Theme, h_flex};
use gpui_kit::*;

use super::registry::{self, ClearClusterTag, SetClusterTag};
use super::theme::cluster_tag_color;

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
    menu.submenu("Colour Tag", window, cx, move |sub, _, _| {
        let sub = ClusterTag::ALL.into_iter().fold(sub, |sub, tag| {
            let label = SharedString::from(tag.label());
            sub.item(
                PopupMenuItem::element(move |_, cx| {
                    h_flex()
                        .id(SharedString::from(format!("menu-cluster-tag-{label}")))
                        .gap_2()
                        .items_center()
                        .role(Role::MenuItem)
                        .aria_label(label.clone())
                        .child(
                            div()
                                .size_2()
                                .rounded_full()
                                .bg(cluster_tag_color(tag, cx.theme())),
                        )
                        .child(label.clone())
                        .test_support()
                })
                .checked(current == Some(tag))
                .on_click(registry::on_click(SetClusterTag {
                    cluster: cluster.clone(),
                    tag,
                    endpoint: Some(endpoint),
                })),
            )
        });
        sub.separator().item(
            PopupMenuItem::new("No Tag")
                .disabled(current.is_none())
                .on_click(registry::on_click(ClearClusterTag {
                    cluster: cluster.clone(),
                    endpoint: Some(endpoint),
                })),
        )
    })
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{AppContext as _, TestAppContext};
    use serde_json::json;

    use super::{ClusterTag, tag_text};
    use crate::ui::support_tests::{WAIT, alpha_and_beta, open_workspace_with};

    // covers UP-83: every colour has its own name, so the tag never relies on colour alone.
    #[test]
    fn every_tag_has_its_own_accessible_name() {
        let mut names: Vec<String> = ClusterTag::ALL.into_iter().map(tag_text).collect();
        assert_eq!(names[5], "Colour tag: Red");
        names.sort();
        names.dedup();
        assert_eq!(names.len(), ClusterTag::ALL.len());
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
        assert_eq!(
            t.label(cx, "cluster-tag-kind-alpha").as_deref(),
            Some("Colour tag: Red")
        );
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
}
