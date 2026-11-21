//! The marker on a list row whose object Argo CD or Flux manages.

use flint_core::Row;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::Theme;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;

/// The short name drawn on the row, from the manager's full text.
fn short_name(manager: &str) -> &'static str {
    if manager.starts_with("Argo CD") {
        "Argo"
    } else {
        "Flux"
    }
}

/// The row's marker, with the full manager in its tooltip. A Secret list shows none: what it would
/// read comes from annotations that are masked there.
pub(super) fn marker(row: &Row, secret_list: bool, theme: &Theme) -> Option<AnyElement> {
    let manager = row.managed_by().filter(|_| !secret_list)?;
    let tip = SharedString::from(format!("Managed by {manager}"));
    Some(
        div()
            .id(SharedString::from(format!("managed-{}", row.uid)))
            .ml_1()
            .px_1()
            .flex_none()
            .rounded_sm()
            .border_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(short_name(manager))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .test_support()
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use flint_core::{
        Cell, Column, ColumnRole, ColumnType, CoreConfig, CoreHandle, KindId, KindSnapshot,
        Settings, SyncState,
    };
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{
        AppContext as _, Bounds, Point, TestAppContext, WindowBounds, WindowOptions, px, size,
    };

    use super::super::ResourceList;
    use super::{Row, short_name};

    const WAIT: Duration = Duration::from_secs(10);

    fn row(name: &str, manager: Option<&str>) -> Arc<Row> {
        let labels: Box<[(Arc<str>, Arc<str>)]> = manager
            .map(|m| (Arc::from(flint_core::gitops::ROW_MANAGER_KEY), Arc::from(m)))
            .into_iter()
            .collect();
        Arc::new(Row {
            uid: format!("uid-{name}").into(),
            name: name.into(),
            namespace: None,
            resource_version: "1".into(),
            generation: None,
            created: Some(0),
            labels,
            cells: Box::new([Cell::Name]),
        })
    }

    fn snapshot(kind: KindId, rows: Vec<Arc<Row>>) -> Arc<KindSnapshot> {
        let columns: Arc<[Column]> = vec![Column {
            name: "Name".into(),
            ty: ColumnType::String,
            format: "".into(),
            description: "".into(),
            priority: 0,
            role: ColumnRole::Name,
        }]
        .into();
        Arc::new(KindSnapshot {
            cluster: "test".into(),
            kind,
            columns,
            rows,
            state: SyncState::Synced,
            version: 1,
            ..Default::default()
        })
    }

    /// Opens a list of `plural` showing `rows`, and returns which uids carry a marker.
    async fn marked(
        cx: &mut TestAppContext,
        plural: &'static str,
        kind: KindId,
        rows: Vec<Arc<Row>>,
    ) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let core = CoreHandle::start(CoreConfig {
            config_dir: dir.path().to_owned(),
            settings: Settings::default(),
            settings_error: None,
            kubeconfig_base: Vec::new(),
            home: None,
            watch_files: false,
            watch_timing: Default::default(),
        })
        .unwrap();
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::ui::registry::init(cx);
        });
        let uids: Vec<String> = rows.iter().map(|r| r.uid.to_string()).collect();
        let (window, list) = cx.update(|cx| {
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(900.), px(600.)),
            };
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        let entry = crate::ui::support_tests::entry("", plural, "Kind", "L", true);
                        let mut list = ResourceList::new(
                            core,
                            "test".into(),
                            &entry,
                            Vec::new(),
                            None,
                            window,
                            cx,
                        );
                        list.stop_watching();
                        list
                    })
                },
            )
            .unwrap()
        });
        let first = format!("row-{}", uids[0]);
        list.update(cx, |list, cx| list.set_snapshot(snapshot(kind, rows), cx));
        cx.wait_for(window, WAIT, move |window, _| {
            window.try_find(first.clone()).is_some()
        })
        .await;
        cx.update_window(window, |_, window, _| {
            uids.iter()
                .filter(|uid| window.try_find(format!("managed-{uid}")).is_some())
                .cloned()
                .collect()
        })
        .unwrap()
    }

    // covers UP-23: Argo CD and Flux objects carry a marker, and an unmanaged one none.
    #[gpui_kit::test]
    async fn managed_rows_carry_a_marker(cx: &mut TestAppContext) {
        let rows = vec![
            row("argo", Some("Argo CD: shop")),
            row("flux", Some("Flux: flux-system/apps")),
            row("plain", None),
        ];
        assert_eq!(
            marked(cx, "pods", KindId::pods(), rows).await,
            ["uid-argo", "uid-flux"]
        );
    }

    // covers UP-10: a Secret list draws nothing read from its masked annotations.
    #[gpui_kit::test]
    async fn a_secret_list_draws_no_marker(cx: &mut TestAppContext) {
        let rows = vec![row("tracked", Some("Argo CD: shop"))];
        let kind = KindId::new("", "secrets");
        assert!(marked(cx, "secrets", kind, rows).await.is_empty());
    }

    #[test]
    fn the_short_name_follows_the_controller() {
        assert_eq!(short_name("Argo CD: shop"), "Argo");
        assert_eq!(short_name("Flux: apps"), "Flux");
        assert_eq!(short_name("Flux HelmRelease: flux-system/web"), "Flux");
    }
}
