//! The sections of the dashboard, drawn from its view model.

// module-tests: dashboard_tests.rs

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::{Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::sparkline::sparkline;
use super::{
    CANNOT_BE_CHECKED, DashboardStatus, INSTALL_COMMAND, INSTALL_HINT, MAX_WARNING_ROWS,
    METRICS_WAITING, MetricTile, MetricsPanel, NO_WARNINGS, OpenProblemList, OpenShortWorkloads,
    ProblemList, ProblemLists, ProblemTile, WARNINGS_UNAVAILABLE, WarningRow, WarningsPanel,
};
use crate::ui::registry;
use flint_core::thresholds::ThresholdLevel;

/// On a tile whose count includes objects still in their grace period.
const STILL_SETTLING: &str = "Some are still settling";

/// The narrowest a tile gets before the row wraps.
const TILE_MIN_WIDTH: f32 = 132.;
const TILE_HEIGHT: f32 = 68.;
const METRIC_MIN_WIDTH: f32 = 220.;
const SPARKLINE_HEIGHT: f32 = 40.;

/// The element id of a problem tile.
pub fn tile_id(tile: &ProblemTile) -> ElementId {
    ("dashboard-tile", tile.category as usize).into()
}

/// The element id of a problem tile's content. It carries the description only when the tile is
/// not a button; a button is named on the control itself.
pub fn tile_content_id(tile: &ProblemTile) -> ElementId {
    ("dashboard-tile-content", tile.category as usize).into()
}

fn section(title: &'static str, theme: &Theme) -> Div {
    v_flex().gap_2().w_full().min_w_0().child(
        div()
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.muted_foreground)
            .child(title),
    )
}

fn note(id: &'static str, text: String, color: Hsla) -> impl IntoElement {
    div()
        .id(id)
        .aria_label(text.clone())
        .text_sm()
        .text_color(color)
        .child(text)
        .test_support()
}

/// The line above the tiles, when something is not fully checked.
pub fn status_banner(status: &DashboardStatus, theme: &Theme) -> Option<impl IntoElement> {
    let (text, color) = match status {
        DashboardStatus::Checked => return None,
        DashboardStatus::Starting => ("Checking this cluster…".to_owned(), theme.muted_foreground),
        DashboardStatus::Partial(text) => (text.clone(), theme.warning),
        DashboardStatus::CannotBeChecked(text) => (text.clone(), theme.danger),
    };
    Some(note("dashboard-status", text, color))
}

fn level_color(level: ThresholdLevel, theme: &Theme) -> Hsla {
    match level {
        ThresholdLevel::Warning => theme.warning,
        ThresholdLevel::Unknown => theme.muted_foreground,
        ThresholdLevel::Healthy => theme.foreground,
    }
}

pub fn problem_tiles(
    tiles: &[ProblemTile],
    lists: &ProblemLists,
    focus: FocusHandle,
    theme: &Theme,
) -> impl IntoElement {
    let mut row = h_flex().flex_wrap().gap_2().w_full();
    let mut by_kind: &[ProblemList] = &[];
    for tile in tiles {
        let drill = lists.get(&tile.category).map(Vec::as_slice);
        if let Some(several @ [_, _, ..]) = drill.filter(|_| tile.opens_list()) {
            by_kind = several;
        }
        row = row.child(problem_tile(tile, drill, focus.clone(), theme));
    }
    section("Problems now", theme)
        .child(row)
        .when(!by_kind.is_empty(), |el| {
            el.child(short_kind_buttons(by_kind, focus))
        })
}

/// One button per kind, for a tile whose objects no single list can show.
fn short_kind_buttons(lists: &[ProblemList], focus: FocusHandle) -> impl IntoElement {
    h_flex()
        .flex_wrap()
        .gap_2()
        .children(lists.iter().enumerate().map(|(index, list)| {
            let text = format!("{} {}", list.filter.objects.len(), list.filter.label);
            Button::new(("dashboard-short-kind", index))
                .accessibility_label(format!("Open {text}"))
                .outline()
                .small()
                .label(text)
                .on_click(registry::on_click_in(
                    focus.clone(),
                    OpenShortWorkloads { kind: list.kind },
                ))
        }))
}

/// `drill` is the tile's lists, when they have been read.
fn problem_tile(
    tile: &ProblemTile,
    drill: Option<&[ProblemList]>,
    focus: FocusHandle,
    theme: &Theme,
) -> AnyElement {
    let is_button = tile.opens_list() && drill.is_none_or(|lists| lists.len() == 1);
    let settling = tile.opens_list() && drill.is_some_and(<[ProblemList]>::is_empty);
    let body = v_flex()
        .id(tile_content_id(tile))
        .when(!is_button, |el| el.aria_label(tile.description()))
        .items_start()
        .w_full()
        .gap_0p5()
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(level_color(tile.level, theme))
                .child(tile.value()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(tile.label),
        )
        .when(!tile.opens_list(), |el| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(CANNOT_BE_CHECKED),
            )
        })
        .when(settling, |el| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(STILL_SETTLING),
            )
        })
        .test_support();
    if is_button {
        Button::new(tile_id(tile))
            .accessibility_label(tile.description())
            .outline()
            .flex_1()
            .min_w(px(TILE_MIN_WIDTH))
            .h(px(TILE_HEIGHT))
            .child(body)
            .on_click(registry::on_click_in(
                focus,
                OpenProblemList {
                    category: tile.category,
                },
            ))
            .into_any_element()
    } else {
        div()
            .flex_1()
            .min_w(px(TILE_MIN_WIDTH))
            .h(px(TILE_HEIGHT))
            .px_3()
            .flex()
            .items_center()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .child(body)
            .into_any_element()
    }
}

pub fn metrics_section(panel: &MetricsPanel, theme: &Theme) -> impl IntoElement {
    let body = match panel {
        MetricsPanel::Waiting => note(
            "dashboard-metrics-waiting",
            METRICS_WAITING.to_owned(),
            theme.muted_foreground,
        )
        .into_any_element(),
        MetricsPanel::NotInstalled => v_flex()
            .id("dashboard-metrics-hint")
            .aria_label(INSTALL_HINT)
            .gap_1()
            .text_sm()
            .child(div().text_color(theme.muted_foreground).child(INSTALL_HINT))
            .child(
                div()
                    .id("dashboard-install-command")
                    .aria_label(INSTALL_COMMAND)
                    .p_2()
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .child(INSTALL_COMMAND)
                    .test_support(),
            )
            .test_support()
            .into_any_element(),
        MetricsPanel::Tiles {
            cpu,
            memory,
            notice,
        } => v_flex()
            .gap_2()
            .when_some(notice.clone(), |el, text| {
                el.child(note("dashboard-metrics-notice", text, theme.warning))
            })
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .w_full()
                    .child(metric_tile("dashboard-cpu", cpu, theme))
                    .child(metric_tile("dashboard-memory", memory, theme)),
            )
            .into_any_element(),
    };
    section("CPU and memory, last hour", theme).child(body)
}

fn metric_tile(id: &'static str, tile: &MetricTile, theme: &Theme) -> impl IntoElement {
    v_flex()
        .id(id)
        .aria_label(tile.description())
        .flex_1()
        .min_w(px(METRIC_MIN_WIDTH))
        .p_3()
        .gap_1()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(tile.label),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(tile.shown_value().to_owned()),
                ),
        )
        .child(
            div()
                .h(px(SPARKLINE_HEIGHT))
                .w_full()
                .child(sparkline(tile.shape.clone(), theme.primary)),
        )
        .test_support()
}

pub fn warnings_section(panel: &WarningsPanel, theme: &Theme) -> impl IntoElement {
    let body = match panel {
        WarningsPanel::Unavailable => note(
            "dashboard-warnings-unavailable",
            WARNINGS_UNAVAILABLE.to_owned(),
            theme.muted_foreground,
        )
        .into_any_element(),
        WarningsPanel::Rows { rows, .. } if rows.is_empty() => note(
            "dashboard-warnings-empty",
            NO_WARNINGS.to_owned(),
            theme.muted_foreground,
        )
        .into_any_element(),
        WarningsPanel::Rows { rows, total } => v_flex()
            .w_full()
            .min_w_0()
            .children(
                rows.iter()
                    .enumerate()
                    .map(|(index, row)| warning_row(index, row, theme)),
            )
            .when(*total > MAX_WARNING_ROWS, |el| {
                el.child(note(
                    "dashboard-warnings-more",
                    format!("Showing the newest {MAX_WARNING_ROWS} of {total}."),
                    theme.muted_foreground,
                ))
            })
            .into_any_element(),
    };
    section("Recent Warning events", theme).child(body)
}

fn warning_row(index: usize, row: &WarningRow, theme: &Theme) -> impl IntoElement {
    h_flex()
        .id(("dashboard-warning", index))
        .aria_label(row.label())
        .w_full()
        .min_w_0()
        .h(px(24.))
        .gap_3()
        .text_xs()
        .child(
            div()
                .w(px(36.))
                .flex_none()
                .text_color(theme.muted_foreground)
                .child(row.age.clone()),
        )
        .child(
            div()
                .w(px(128.))
                .flex_none()
                .truncate()
                .text_color(theme.warning)
                .child(row.reason.clone()),
        )
        .child(
            div()
                .w(px(200.))
                .flex_shrink(1.)
                .min_w_0()
                .truncate()
                .child(row.object.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme.muted_foreground)
                .child(row.message.clone()),
        )
        .when(row.occurrences > 1, |el| {
            el.child(
                div()
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child(format!("x{}", row.occurrences)),
            )
        })
        .test_support()
}
