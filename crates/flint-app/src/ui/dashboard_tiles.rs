//! The sections of the dashboard, drawn from its view model.

// module-tests: dashboard_tests.rs

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::overview::{
    CAPACITY_UNKNOWN, CapacityCard, CapacityPanel, InventoryCount, InventoryPanel, MAX_NODE_ROWS,
    MAX_PROBLEM_ROWS, NO_PROBLEMS, NO_PROBLEMS_SO_FAR, NodeRow, PROBLEMS_UNKNOWN, ProblemRow,
    ProblemsPanel,
};
use super::sparkline::sparkline;
use super::{
    CANNOT_BE_CHECKED, DashboardModel, DashboardStatus, DrawnLinks, INSTALL_COMMAND, INSTALL_HINT,
    MAX_WARNING_ROWS, METRICS_WAITING, MetricTile, MetricsPanel, NO_WARNINGS, OpenProblemList,
    OpenShortWorkloads, ProblemList, ProblemLists, ProblemTile, WARNINGS_UNAVAILABLE, WarningRow,
    WarningsPanel,
};
use crate::ui::registry;
use flint_core::problems::ProblemSeverity;
use flint_core::rollout::RolloutState;
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

/// The line above the tiles, when something is not fully checked. Its place is kept when there is
/// nothing to say, so the tiles do not move when "Checking this cluster…" goes away.
pub fn status_banner(status: &DashboardStatus, theme: &Theme) -> impl IntoElement {
    let line = match status {
        DashboardStatus::Checked => None,
        DashboardStatus::Starting => {
            Some(("Checking this cluster…".to_owned(), theme.muted_foreground))
        }
        DashboardStatus::Partial(text) => Some((text.clone(), theme.warning)),
        DashboardStatus::CannotBeChecked(text) => Some((text.clone(), theme.danger)),
    };
    div()
        .w_full()
        .flex_none()
        .min_h(STATUS_LINE_HEIGHT)
        .when_some(line, |slot, (text, color)| {
            slot.child(note("dashboard-status", text, color))
        })
}

/// One `text_sm` line.
const STATUS_LINE_HEIGHT: Pixels = px(20.);

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
    section("What's wrong", theme)
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

/// The individual problems, each opening its object on this cluster.
pub fn problems_section(
    panel: &ProblemsPanel,
    links: &DrawnLinks,
    theme: &Theme,
) -> impl IntoElement {
    let muted = theme.muted_foreground;
    let body = match panel {
        ProblemsPanel::Unknown => note(
            "dashboard-problems-unknown",
            PROBLEMS_UNKNOWN.to_owned(),
            muted,
        )
        .into_any_element(),
        ProblemsPanel::Rows {
            rows,
            total,
            settling,
            incomplete,
        } => {
            let empty = incomplete_or_clear(*incomplete);
            v_flex()
                .w_full()
                .min_w_0()
                .when(rows.is_empty(), |el| {
                    el.child(note("dashboard-problems-empty", empty.to_owned(), muted))
                })
                .children(
                    rows.iter()
                        .enumerate()
                        .map(|(index, row)| problem_row(index, row, links, theme)),
                )
                .when(*total > MAX_PROBLEM_ROWS, |el| {
                    el.child(note(
                        "dashboard-problems-more",
                        format!("Showing the {MAX_PROBLEM_ROWS} most urgent of {total}."),
                        muted,
                    ))
                })
                .when(*settling > 0, |el| {
                    el.child(note(
                        "dashboard-problems-settling",
                        format!("{settling} more may still settle by themselves."),
                        muted,
                    ))
                })
                .when(*incomplete && !rows.is_empty(), |el| {
                    el.child(note(
                        "dashboard-problems-incomplete",
                        "Some kinds could not be checked, so there may be more.".to_owned(),
                        theme.warning,
                    ))
                })
                .into_any_element()
        }
    };
    section("Problems by severity", theme).child(body)
}

const fn incomplete_or_clear(incomplete: bool) -> &'static str {
    if incomplete {
        NO_PROBLEMS_SO_FAR
    } else {
        NO_PROBLEMS
    }
}

fn problem_row(
    index: usize,
    row: &ProblemRow,
    links: &DrawnLinks,
    theme: &Theme,
) -> impl IntoElement {
    let color = match row.severity {
        ProblemSeverity::Critical => theme.danger,
        ProblemSeverity::Warning => theme.warning,
    };
    let severity = match row.severity {
        ProblemSeverity::Critical => "Critical",
        ProblemSeverity::Warning => "Warning",
    };
    Button::new(("dashboard-problem", index))
        .accessibility_label(format!("Open {}", row.label()))
        .ghost()
        .small()
        .w_full()
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_3()
                .text_xs()
                .child(
                    div()
                        .w(px(56.))
                        .flex_none()
                        .text_color(color)
                        .child(severity),
                )
                .child(
                    div()
                        .w(px(240.))
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
                        .child(row.summary),
                ),
        )
        .on_click(links.on_click(
            row.open.clone(),
            DashboardModel::links_object,
            registry::on_click_in(links.focus.clone(), row.open.clone()),
        ))
}

/// What is left of each resource, then the nodes, most allocated first.
pub fn capacity_section(panel: &CapacityPanel, theme: &Theme) -> impl IntoElement {
    let body = match panel {
        CapacityPanel::Unknown => note(
            "dashboard-capacity-unknown",
            CAPACITY_UNKNOWN.to_owned(),
            theme.muted_foreground,
        )
        .into_any_element(),
        CapacityPanel::Known {
            cards,
            rows,
            total_nodes,
            usage_note,
        } => v_flex()
            .gap_2()
            .w_full()
            .min_w_0()
            .child(
                h_flex().flex_wrap().gap_2().w_full().children(
                    cards
                        .iter()
                        .enumerate()
                        .map(|(index, card)| capacity_card(index, card, theme)),
                ),
            )
            .when_some(usage_note.clone(), |el, text| {
                el.child(note("dashboard-usage-note", text, theme.warning))
            })
            .when(!rows.is_empty(), |el| {
                el.child(allocation_table(rows, theme))
            })
            .when(*total_nodes > MAX_NODE_ROWS, |el| {
                el.child(note(
                    "dashboard-nodes-more",
                    format!("Showing the {MAX_NODE_ROWS} most allocated of {total_nodes} nodes."),
                    theme.muted_foreground,
                ))
            })
            .into_any_element(),
    };
    section("Capacity left", theme).child(body)
}

fn capacity_line(label: &'static str, value: String, theme: &Theme) -> impl IntoElement {
    h_flex()
        .justify_between()
        .gap_2()
        .text_xs()
        .child(div().text_color(theme.muted_foreground).child(label))
        .child(div().truncate().child(value))
}

fn capacity_card(index: usize, card: &CapacityCard, theme: &Theme) -> impl IntoElement {
    let filled = card
        .requested_percent
        .map_or(0., |p| p.min(100) as f32 / 100.);
    let over = card.requested_percent.is_some_and(|p| p > 100);
    v_flex()
        .id(("dashboard-capacity", index))
        .aria_label(card.description())
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
                        .child(card.label),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{} left", card.left)),
                ),
        )
        .child(
            div()
                .h(px(4.))
                .w_full()
                .rounded(theme.radius)
                .bg(theme.muted)
                .child(
                    div()
                        .h_full()
                        .w(relative(filled))
                        .rounded(theme.radius)
                        .bg(if over { theme.warning } else { theme.primary }),
                ),
        )
        .child(capacity_line(
            "Allocatable",
            card.allocatable.clone(),
            theme,
        ))
        .child(capacity_line("Requested", card.requested.clone(), theme))
        .when_some(card.used.clone(), |el, used| {
            el.child(capacity_line("Used now", used, theme))
        })
        .when_some(card.limits.clone(), |el, limits| {
            el.child(capacity_line("Limits", limits, theme))
        })
        .test_support()
}

fn table_cell(text: impl Into<SharedString>) -> Div {
    div().flex_1().min_w_0().truncate().child(text.into())
}

fn allocation_table(rows: &[NodeRow], theme: &Theme) -> impl IntoElement {
    let header = h_flex()
        .w_full()
        .min_w_0()
        .gap_3()
        .h(px(24.))
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(table_cell("Node"))
        .child(table_cell("CPU requested"))
        .child(table_cell("Memory requested"))
        .child(table_cell("Pods"))
        .child(table_cell("CPU used"))
        .child(table_cell("Memory used"));
    v_flex()
        .id("dashboard-allocation")
        .aria_label("Allocation by node, most allocated first")
        .w_full()
        .min_w_0()
        .child(header)
        .children(rows.iter().enumerate().map(|(index, row)| {
            h_flex()
                .id(("dashboard-node", index))
                .aria_label(row.label())
                .w_full()
                .min_w_0()
                .gap_3()
                .h(px(24.))
                .text_xs()
                .child(table_cell(row.name.clone()))
                .child(table_cell(row.cpu_requested.clone()))
                .child(table_cell(row.memory_requested.clone()))
                .child(table_cell(row.pods.clone()))
                .child(table_cell(row.cpu_used.clone()))
                .child(table_cell(row.memory_used.clone()))
                .test_support()
        }))
        .test_support()
}

/// The version, and a count per kind that opens its list.
pub fn inventory_section(
    panel: &InventoryPanel,
    links: &DrawnLinks,
    theme: &Theme,
) -> impl IntoElement {
    let version = format!("Kubernetes {}", panel.version);
    section("What's in the cluster", theme)
        .child(note("dashboard-version", version, theme.foreground))
        .child(
            h_flex().flex_wrap().gap_2().w_full().children(
                panel
                    .counts
                    .iter()
                    .enumerate()
                    .map(|(index, count)| inventory_count(index, count, links, theme)),
            ),
        )
}

fn inventory_count(
    index: usize,
    count: &InventoryCount,
    links: &DrawnLinks,
    theme: &Theme,
) -> AnyElement {
    let text = format!("{} {}", count.value(), count.label);
    if count.count.is_some() {
        Button::new(("dashboard-count", index))
            .accessibility_label(format!("Open {}", count.description()))
            .outline()
            .small()
            .label(text)
            .on_click(links.on_click(
                count.open.clone(),
                DashboardModel::links_view,
                registry::on_click_in(links.focus.clone(), count.open.clone()),
            ))
            .into_any_element()
    } else {
        div()
            .id(("dashboard-count-unknown", index))
            .aria_label(count.description())
            .px_2()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(text)
            .test_support()
            .into_any_element()
    }
}

pub fn warnings_section(
    panel: &WarningsPanel,
    links: &DrawnLinks,
    theme: &Theme,
) -> impl IntoElement {
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
            .children(rows.iter().map(|row| warning_row(row, links, theme)))
            .when(*total > MAX_WARNING_ROWS, |el| {
                el.child(note(
                    "dashboard-warnings-more",
                    format!("Showing the newest {MAX_WARNING_ROWS} of {total}."),
                    theme.muted_foreground,
                ))
            })
            .into_any_element(),
    };
    section("Recent activity", theme).child(body)
}

/// A row that opens its object, or plain text when the event names none exactly. Its id follows
/// the event, so keyboard focus stays on it as rows above come and go.
fn warning_row(row: &WarningRow, links: &DrawnLinks, theme: &Theme) -> AnyElement {
    let id = ElementId::Name(format!("dashboard-warning-{}", row.key).into());
    let Some(open) = row.open.clone() else {
        return warning_cells(row, theme)
            .id(id)
            .aria_label(row.label())
            .test_support()
            .into_any_element();
    };
    Button::new(id)
        .accessibility_label(format!("Open {}", row.label()))
        .ghost()
        .small()
        .w_full()
        .child(warning_cells(row, theme))
        .on_click(links.on_warning_click(row, open, registry::dispatch_in))
        .into_any_element()
}

fn warning_cells(row: &WarningRow, theme: &Theme) -> Div {
    h_flex()
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
                .text_color(match row.rollout {
                    None | Some(RolloutState::Started | RolloutState::Progressing) => theme.warning,
                    Some(RolloutState::Complete) => theme.success,
                    Some(RolloutState::Stalled) => theme.danger,
                })
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
}
