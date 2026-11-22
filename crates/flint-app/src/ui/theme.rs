//! Flint Light and Flint Dark, and which of them is shown (ARCHITECTURE.md, "Look and keyboard").
//!
//! The themes are gpui-component theme data (`themes/flint.json`, embedded at build time) with
//! the orange accent from [`super::accent`] applied on top. Views never name a color: they read
//! theme tokens (`cx.theme()`), so this module and the theme data are the only places colors live
//! (`scripts/check-theme-colors.sh` enforces it).

use std::rc::Rc;

use flint_core::Appearance;
use flint_core::settings::ClusterTag;
use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode, ThemeSet};
use gpui_kit::{App, Div, Hsla, Pixels, SharedString, Styled as _, WindowAppearance, div, px};

use super::accent::{ACCENT, Accent, Shades};

/// Both themes, without the accent.
pub(super) const THEMES: &str = include_str!("themes/flint.json");

/// Flint Light and Flint Dark with `accent` applied.
pub fn themes(accent: &Accent) -> (ThemeConfig, ThemeConfig) {
    let set: ThemeSet = serde_json::from_str(THEMES).expect("themes/flint.json is a theme set");
    let (mut light, mut dark) = (None, None);
    for mut theme in set.themes {
        if theme.mode.is_dark() {
            apply_accent(&mut theme.colors, &accent.dark);
            dark = Some(theme);
        } else {
            apply_accent(&mut theme.colors, &accent.light);
            light = Some(theme);
        }
    }
    (
        light.expect("themes/flint.json has a light theme"),
        dark.expect("themes/flint.json has a dark theme"),
    )
}

/// The tokens the accent owns. The theme file leaves them out, so changing [`ACCENT`] changes
/// every one of them.
fn apply_accent(colors: &mut ThemeConfigColors, shades: &Shades) {
    let c = |value: &str| Some(SharedString::from(value.to_owned()));
    // `#rrggbb` plus an alpha byte.
    let a = |value: &str, alpha: &str| Some(SharedString::from(format!("{value}{alpha}")));
    colors.primary = c(shades.fill);
    colors.primary_hover = c(shades.hover);
    colors.primary_active = c(shades.active);
    colors.primary_foreground = c(shades.on_fill);
    colors.button_primary = c(shades.fill);
    colors.button_primary_hover = c(shades.hover);
    colors.button_primary_active = c(shades.active);
    colors.button_primary_foreground = c(shades.on_fill);
    colors.sidebar_primary = c(shades.fill);
    colors.sidebar_primary_foreground = c(shades.on_fill);
    colors.ring = c(shades.fill);
    colors.caret = c(shades.fill);
    colors.drag_border = c(shades.fill);
    colors.drop_target = a(shades.fill, "33");
    // Selection: a tint behind normal text, light enough to keep syntax colors readable, plus
    // an accent edge (`*_active_border`, see `selection_edge`) on selected rows and items.
    colors.selection = a(shades.fill, "33");
    colors.list_active = a(shades.fill, "24");
    colors.list_active_border = c(shades.fill);
    colors.table_active = a(shades.fill, "24");
    colors.table_active_border = c(shades.fill);
    colors.list_highlight_foreground = c(shades.text);
    colors.link = c(shades.text);
    colors.link_hover = c(shades.hover);
    colors.link_active = c(shades.active);
    colors.progress_bar = c(shades.fill);
    colors.slider_bar = c(shades.fill);
    colors.slider_thumb = c(shades.on_fill);
    colors.chart_1 = c(shades.fill);
}

/// The border colour of a row the keyboard cursor can be on (the cluster sidebar, the
/// navigation): the theme's focus ring on the cursor's row, else the same ring fully
/// transparent. Every such row keeps a 1 px border, so moving the ring shifts nothing.
pub fn cursor_ring(theme: &Theme, on: bool) -> Hsla {
    if on {
        theme.ring
    } else {
        theme.ring.opacity(0.)
    }
}

/// The focus ring around a whole pane that has keyboard focus (the list, the detail pane):
/// the cursor ring's colour on an overlay drawn over the pane's edge, so the pane's layout does
/// not change with focus and clicks go to what is under it. Put it last in a `relative()` pane.
pub fn pane_focus_ring(theme: &Theme) -> Div {
    div()
        .absolute()
        .inset_0()
        .border_1()
        .border_color(cursor_ring(theme, true))
}

/// The accent edge that marks a selected row or item, drawn beside its tint: the tint alone is too
/// faint to tell selected apart. Put it in a `relative()` element; `inset` keeps it off the
/// element's top and bottom, so it sits clear of rounded corners.
pub fn selection_edge(color: Hsla, inset: Pixels) -> Div {
    div()
        .absolute()
        .left_0()
        .top(inset)
        .bottom(inset)
        .w(px(2.))
        .rounded_full()
        .bg(color)
}

/// A small status dot: filled with `color`, or a ring of it where `filled` is false (the nav's
/// loading kinds, an idle cluster). Callers add their own spacing, `flex_none` in a row that may
/// shrink, and a larger size where they want one.
pub fn status_dot(color: Hsla, filled: bool) -> Div {
    let dot = div().size_1p5().rounded_full();
    if filled {
        dot.bg(color)
    } else {
        dot.border_1().border_color(color)
    }
}

/// The background of a search match in a log line: a tint of the warning color, as editors
/// mark matches, so it never reads as the (accent) selection.
pub fn log_match(theme: &Theme) -> Hsla {
    theme.warning.opacity(0.16)
}

/// The background of the current search match (where next and previous move): a stronger
/// tint than the other matches; the view also draws it bold and underlined in the warning color.
pub fn log_current_match(theme: &Theme) -> Hsla {
    // Dark's warning is bright: a stronger tint would wash out the light text over it.
    theme
        .warning
        .opacity(if theme.mode.is_dark() { 0.3 } else { 0.42 })
}

/// The background of the line holding the current search match, so it stands out from the
/// other matching lines at a glance.
pub fn log_current_line(theme: &Theme) -> Hsla {
    theme.warning.opacity(0.07)
}

/// The colours of workload pod prefixes in a log tab, one per pod in the order pods are first
/// seen (#62). Red and yellow are left out: they mark error and warning lines.
pub fn log_source_colors(theme: &Theme) -> [Hsla; 4] {
    // In the light theme a shade darker, so a prefix stays readable on a selected line's tint.
    let shade = |c: Hsla| {
        if theme.mode.is_dark() {
            c
        } else {
            Hsla {
                l: (c.l - 0.06).max(0.),
                ..c
            }
        }
    };
    [theme.blue, theme.magenta, theme.cyan, theme.green].map(shade)
}

/// The colour of a cluster's tag, from the theme's base palette.
pub fn cluster_tag_color(tag: ClusterTag, theme: &Theme) -> Hsla {
    match tag {
        ClusterTag::Blue => theme.blue,
        ClusterTag::Purple => theme.magenta,
        ClusterTag::Teal => theme.cyan,
        ClusterTag::Green => theme.green,
        ClusterTag::Yellow => theme.yellow,
        ClusterTag::Red => theme.red,
    }
}

/// Installs Flint Light and Flint Dark in place of gpui-component's defaults, showing `mode`.
/// Call after `gpui_kit::init`.
pub fn init(mode: ThemeMode, cx: &mut App) {
    let (light, dark) = themes(&ACCENT);
    let theme = Theme::global_mut(cx);
    theme.light_theme = Rc::new(light);
    theme.dark_theme = Rc::new(dark);
    Theme::change(mode, None, cx);
}

/// The appearance to force on the native window chrome (titlebar, border), so it matches the
/// theme: `None` follows macOS.
pub fn chrome(setting: Appearance, forced: Option<ThemeMode>) -> Option<WindowAppearance> {
    let mode = match (forced, setting) {
        (Some(mode), _) => mode,
        (None, Appearance::Light) => ThemeMode::Light,
        (None, Appearance::Dark) => ThemeMode::Dark,
        (None, Appearance::System) => return None,
    };
    Some(if mode.is_dark() {
        WindowAppearance::Dark
    } else {
        WindowAppearance::Light
    })
}

/// The theme to show: `FLINT_APPEARANCE` (a development aid) wins, then the `appearance`
/// setting, then macOS.
pub fn resolve(
    setting: Appearance,
    forced: Option<ThemeMode>,
    system: WindowAppearance,
) -> ThemeMode {
    match (forced, setting) {
        (Some(mode), _) => mode,
        (None, Appearance::Light) => ThemeMode::Light,
        (None, Appearance::Dark) => ThemeMode::Dark,
        (None, Appearance::System) => ThemeMode::from(system),
    }
}

/// Shows `mode`. Does nothing when it already shows, so it is cheap to call on every change of
/// settings or of the macOS appearance.
pub fn show(mode: ThemeMode, cx: &mut App) {
    if Theme::global(cx).mode != mode {
        Theme::change(mode, None, cx);
    }
}

#[cfg(test)]
pub(super) mod tests {
    // Not `super::*`: that brings in `gpui_kit::test`, which shadows the std `#[test]`.
    use super::{
        THEMES, chrome, cursor_ring, log_current_line, log_current_match, log_match,
        log_source_colors, resolve, status_dot, themes,
    };
    use crate::ui::accent::{ACCENT, CANDIDATES};
    use flint_core::Appearance;
    use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode, try_parse_color};
    use gpui_kit::{Hsla, InteractiveElement as _, Rgba, Styled as _, WindowAppearance, div, hsla};
    use std::rc::Rc;

    // The keyboard cursor's border is the focus ring, and invisible off the cursor (UP-33).
    #[test]
    fn the_cursor_ring_is_the_focus_ring_or_nothing() {
        let theme = Theme::default();
        assert_eq!(cursor_ring(&theme, true), theme.ring);
        assert_eq!(cursor_ring(&theme, false).a, 0.);
    }

    // Each status dot draws exactly as the hand-built circle it replaced: the nav's dot and
    // ring, the list's, the log and shell tabs' and the dock tab's state dots, the detail
    // pane's condition rows, and the cluster row's dot.
    #[test]
    fn status_dots_draw_as_the_circles_they_replaced() {
        let color = hsla(0.4, 0.6, 0.5, 1.);
        // The nav's dot and ring.
        assert_eq!(
            status_dot(color, true).mx_1().flex_none().style(),
            div()
                .size_1p5()
                .mx_1()
                .flex_none()
                .rounded_full()
                .bg(color)
                .style()
        );
        assert_eq!(
            status_dot(color, false).mx_1().flex_none().style(),
            div()
                .size_1p5()
                .mx_1()
                .flex_none()
                .rounded_full()
                .border_1()
                .border_color(color)
                .style()
        );
        // The list's sync state.
        assert_eq!(
            status_dot(color, true).style(),
            div().size_1p5().rounded_full().bg(color).style()
        );
        // The log and shell tabs' state, the dock tab's dot and the detail pane's conditions.
        assert_eq!(
            status_dot(color, true).flex_none().style(),
            div()
                .size_1p5()
                .flex_none()
                .rounded_full()
                .bg(color)
                .style()
        );
        // The cluster row: larger, filled or a ring.
        assert_eq!(
            status_dot(color, true).id("d").size_2().flex_none().style(),
            div()
                .id("d")
                .size_2()
                .flex_none()
                .rounded_full()
                .bg(color)
                .style()
        );
        assert_eq!(
            status_dot(color, false)
                .id("d")
                .size_2()
                .flex_none()
                .style(),
            div()
                .id("d")
                .size_2()
                .flex_none()
                .rounded_full()
                .border_1()
                .border_color(color)
                .style()
        );
        assert_ne!(
            status_dot(color, true).style(),
            status_dot(color, false).style()
        );
    }

    const LIGHT_NAME: &str = "Flint Light";
    const DARK_NAME: &str = "Flint Dark";

    /// The tokens a theme file can set, as named in the file.
    fn known_tokens() -> Vec<String> {
        let value = serde_json::to_value(ThemeConfigColors::default()).unwrap();
        value.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn the_theme_file_names_only_real_tokens_and_leaves_the_accent_to_code() {
        let file: serde_json::Value = serde_json::from_str(THEMES).unwrap();
        let known = known_tokens();
        let (light, _) = themes(&ACCENT);
        let accent_owned: Vec<String> = {
            let bare = serde_json::to_value(ThemeConfigColors::default()).unwrap();
            let with_accent = serde_json::to_value(&light.colors).unwrap();
            // A token the file leaves empty but the accent fills.
            let file_light = &file["themes"][0]["colors"];
            known
                .iter()
                .filter(|k| bare[k.as_str()].is_null() && file_light.get(k.as_str()).is_none())
                .filter(|k| !with_accent[k.as_str()].is_null())
                .cloned()
                .collect()
        };
        assert!(accent_owned.iter().any(|k| k == "primary.background"));
        for theme in file["themes"].as_array().unwrap() {
            let name = theme["name"].as_str().unwrap();
            for key in theme["colors"].as_object().unwrap().keys() {
                // An unknown key would be ignored silently: a typo would leave a default color.
                assert!(known.contains(key), "{name}: unknown token {key}");
            }
            for key in &accent_owned {
                assert!(
                    theme["colors"].get(key.as_str()).is_none(),
                    "{name}: {key} belongs to the accent"
                );
            }
        }
    }

    // covers UP-32
    #[test]
    fn every_token_is_set_in_both_themes_with_every_accent() {
        for accent in CANDIDATES {
            let (light, dark) = themes(&accent);
            assert_eq!((&*light.name, light.mode), (LIGHT_NAME, ThemeMode::Light));
            assert_eq!((&*dark.name, dark.mode), (DARK_NAME, ThemeMode::Dark));
            for theme in [light, dark] {
                let colors = serde_json::to_value(&theme.colors).unwrap();
                for (key, value) in colors.as_object().unwrap() {
                    let value = value.as_str().unwrap_or_else(|| {
                        panic!("{} ({}): {key} is not set", theme.name, accent.name)
                    });
                    // An invalid color falls back to a default silently.
                    assert!(
                        try_parse_color(value).is_ok(),
                        "{}: {key} = {value} is not a color",
                        theme.name
                    );
                }
                assert!(
                    theme.highlight.is_some(),
                    "{}: no syntax colors",
                    theme.name
                );
                assert!(theme.font_size.is_some() && theme.radius.is_some());
            }
        }
    }

    // covers UP-13, UP-32
    #[test]
    fn the_setting_overrides_macos_and_flint_appearance_overrides_both() {
        use Appearance::*;
        let (light, dark) = (WindowAppearance::Light, WindowAppearance::Dark);
        assert_eq!(resolve(System, None, dark), ThemeMode::Dark);
        assert_eq!(resolve(System, None, light), ThemeMode::Light);
        assert_eq!(
            resolve(System, None, WindowAppearance::VibrantDark),
            ThemeMode::Dark
        );
        assert_eq!(resolve(Light, None, dark), ThemeMode::Light);
        assert_eq!(resolve(Dark, None, light), ThemeMode::Dark);
        assert_eq!(
            resolve(Dark, Some(ThemeMode::Light), dark),
            ThemeMode::Light
        );
        assert_eq!(
            resolve(System, Some(ThemeMode::Dark), light),
            ThemeMode::Dark
        );
    }

    // covers UP-32
    #[test]
    fn the_window_chrome_is_forced_only_away_from_system() {
        use Appearance::*;
        assert_eq!(chrome(System, None), None);
        assert_eq!(chrome(Light, None), Some(WindowAppearance::Light));
        assert_eq!(chrome(Dark, None), Some(WindowAppearance::Dark));
        assert_eq!(
            chrome(System, Some(ThemeMode::Dark)),
            Some(WindowAppearance::Dark)
        );
        assert_eq!(
            chrome(Dark, Some(ThemeMode::Light)),
            Some(WindowAppearance::Light)
        );
    }

    /// The colors a view sees for `config`.
    pub(in crate::ui) fn resolved(config: ThemeConfig) -> Theme {
        let mut theme = Theme::default();
        theme.apply_config(&Rc::new(config));
        theme
    }

    fn rgb(color: Hsla) -> [f32; 4] {
        let c = Rgba::from(color);
        [c.r, c.g, c.b, c.a]
    }

    /// `top` drawn over the opaque `bottom`.
    fn over(top: Hsla, bottom: Hsla) -> [f32; 3] {
        let (t, b) = (rgb(top), rgb(bottom));
        let a = t[3];
        [0, 1, 2].map(|i| t[i] * a + b[i] * (1. - a))
    }

    /// WCAG 2 relative luminance.
    fn luminance([r, g, b]: [f32; 3]) -> f32 {
        let f = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
    }

    /// The WCAG 2 contrast ratio of `fg` over `bg`, both drawn over `base` (the opaque surface
    /// below translucent tokens).
    pub(in crate::ui) fn contrast(fg: Hsla, bg: Hsla, base: Hsla) -> f32 {
        let base = over(base, base);
        let bg = over(
            bg,
            Hsla::from(Rgba {
                r: base[0],
                g: base[1],
                b: base[2],
                a: 1.,
            }),
        );
        let bg_hsla = Hsla::from(Rgba {
            r: bg[0],
            g: bg[1],
            b: bg[2],
            a: 1.,
        });
        let (a, b) = (luminance(over(fg, bg_hsla)), luminance(bg));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    const BODY: f32 = 4.5;
    const UI: f32 = 3.0;

    /// Checks every text-on-surface, status and accent pair of one resolved theme.
    fn check(t: &Theme, accent: &str) -> Vec<String> {
        let mut failures = Vec::new();
        let mut need = |what: &str, fg: Hsla, bg: Hsla, base: Hsla, min: f32| {
            let ratio = contrast(fg, bg, base);
            if ratio < min {
                failures.push(format!(
                    "{} / {accent}: {what} is {ratio:.2}:1, needs {min}:1",
                    t.theme_name()
                ));
            }
        };
        let base = t.background;
        // Surfaces body text sits on, each with the base it is drawn over.
        let surfaces = [
            ("background", t.background, base),
            ("sidebar", t.sidebar, base),
            ("popover", t.popover, base),
            ("muted", t.muted, base),
            ("hover", t.accent, base),
            ("list hover", t.list_hover, base),
            ("table even row", t.table_even, base),
            ("table hover", t.table_hover, base),
            ("table head", t.table_head, base),
            ("selected row", t.table_active, base),
            ("selected list item", t.list_active, base),
            ("selected item in sidebar", t.list_active, t.sidebar),
        ];
        for (name, bg, under) in surfaces {
            need(&format!("text on {name}"), t.foreground, bg, under, BODY);
            need(
                &format!("muted text on {name}"),
                t.muted_foreground,
                bg,
                under,
                BODY,
            );
        }
        need("sidebar text", t.sidebar_foreground, t.sidebar, base, BODY);
        need("popover text", t.popover_foreground, t.popover, base, BODY);
        need(
            "column headers",
            t.table_head_foreground,
            t.table_head,
            base,
            BODY,
        );
        need("tab", t.tab_foreground, t.tab_bar, base, BODY);
        need(
            "active tab",
            t.tab_active_foreground,
            t.tab_active,
            base,
            BODY,
        );
        need("button text", t.button_foreground, t.button, base, BODY);
        need(
            "secondary button text",
            t.secondary_foreground,
            t.secondary,
            base,
            BODY,
        );

        // Status colors: as text on every main surface and on their own notice tint, as a solid
        // tag with its foreground, and as a tinted button.
        let statuses = [
            (
                "success",
                t.success,
                t.success_foreground,
                t.button_success,
                t.button_success_foreground,
            ),
            (
                "warning",
                t.warning,
                t.warning_foreground,
                t.button_warning,
                t.button_warning_foreground,
            ),
            (
                "danger",
                t.danger,
                t.danger_foreground,
                t.button_danger,
                t.button_danger_foreground,
            ),
            (
                "info",
                t.info,
                t.info_foreground,
                t.button_info,
                t.button_info_foreground,
            ),
        ];
        for (name, color, on_color, button, button_fg) in statuses {
            for (surface, bg, under) in [
                ("background", t.background, base),
                ("sidebar", t.sidebar, base),
                ("popover", t.popover, base),
                ("table even row", t.table_even, base),
                // Status cells in the resource list (`list_model::cell_tone`).
                ("table hover", t.table_hover, base),
                ("selected row", t.table_active, base),
            ] {
                need(&format!("{name} text on {surface}"), color, bg, under, BODY);
            }
            // `notice` draws the status color over an 8% tint of itself.
            need(
                &format!("{name} notice"),
                color,
                color.opacity(0.08),
                base,
                BODY,
            );
            need(&format!("{name} tag"), on_color, color, base, BODY);
            need(&format!("{name} button"), button_fg, button, base, BODY);
            need(
                &format!("{name} mark on sidebar"),
                color,
                t.sidebar,
                base,
                UI,
            );
        }

        // The accent: text on its fill, links on surfaces, and marks against the surfaces they
        // sit on.
        need(
            "primary button",
            t.primary_foreground,
            t.primary,
            base,
            BODY,
        );
        need(
            "primary button hovered",
            t.primary_foreground,
            t.primary_hover,
            base,
            BODY,
        );
        need(
            "primary button pressed",
            t.primary_foreground,
            t.primary_active,
            base,
            BODY,
        );
        for (surface, bg) in [
            ("background", t.background),
            ("sidebar", t.sidebar),
            ("popover", t.popover),
        ] {
            need(&format!("link on {surface}"), t.link, bg, base, BODY);
            need(&format!("focus ring on {surface}"), t.ring, bg, base, UI);
            need(
                &format!("accent mark on {surface}"),
                t.primary,
                bg,
                base,
                UI,
            );
        }
        // The active cluster: its row is tinted, and its name, host and status marks sit on it.
        let active_cluster = t.sidebar.blend(t.list_active);
        need(
            "cluster name on the active cluster",
            t.sidebar_foreground,
            active_cluster,
            base,
            BODY,
        );
        need(
            "warning tag on the active cluster",
            t.warning,
            active_cluster,
            base,
            BODY,
        );
        // Selection is a tint plus an accent edge; the edge is what tells selected apart.
        need(
            "selected item edge",
            t.list_active_border,
            t.sidebar,
            base,
            UI,
        );
        need("selected item edge", t.list_active_border, base, base, UI);
        need("selected row edge", t.table_active_border, base, base, UI);
        need(
            "connected mark on the active cluster",
            t.success,
            active_cluster,
            base,
            UI,
        );
        // The disconnected dot is the row's only mark of that state.
        let (not_connected, _) = crate::ui::clusters::Presence::NotConnected.dot(t);
        for (surface, bg) in [("sidebar", t.sidebar), ("active cluster", active_cluster)] {
            need(
                &format!("disconnected mark on the {surface}"),
                not_connected,
                bg,
                base,
                UI,
            );
        }
        // Log lines (`log_view`): error and warning lines and timestamps on the background and
        // on a selected line are checked above (status text and muted text on the selected
        // row). Matched text is drawn in the foreground color over the match tint, also inside
        // a selected line.
        let selected_line = base.blend(t.table_active);
        let current_line = base.blend(log_current_line(t));
        for (bg_name, bg, under) in [
            ("search match", log_match(t), base),
            ("current match", log_current_match(t), current_line),
            (
                "search match on the current line",
                log_match(t),
                current_line,
            ),
            (
                "search match in a selected line",
                log_match(t),
                selected_line,
            ),
            (
                "current match in a selected line",
                log_current_match(t),
                selected_line,
            ),
        ] {
            need(
                &format!("log text on {bg_name}"),
                t.foreground,
                bg,
                under,
                BODY,
            );
        }
        // Lines on the current match's line, and workload pod prefixes (one colour per pod) on
        // the background, the current line and a selected line.
        need(
            "log text on the current match's line",
            t.foreground,
            log_current_line(t),
            base,
            BODY,
        );
        for (i, color) in log_source_colors(t).into_iter().enumerate() {
            for (bg_name, bg) in [
                ("the background", base),
                ("a selected line", selected_line),
                ("the current match's line", current_line),
            ] {
                need(
                    &format!("log pod prefix {i} on {bg_name}"),
                    color,
                    bg,
                    base,
                    BODY,
                );
            }
        }
        failures
    }

    // covers UP-32
    #[test]
    fn text_status_and_accent_colors_are_readable_in_both_themes() {
        let mut failures = Vec::new();
        for accent in CANDIDATES {
            let (light, dark) = themes(&accent);
            failures.extend(check(&resolved(light), accent.name));
            failures.extend(check(&resolved(dark), accent.name));
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// The YAML view's colors: every syntax color is body text on the editor background, and the
    /// file names only keys gpui-component reads.
    // covers UP-32
    #[test]
    fn syntax_colors_are_known_and_readable() {
        use gpui_kit::component::highlighter::HighlightThemeStyle;
        let known = serde_json::to_value(HighlightThemeStyle::default()).unwrap();
        let file: serde_json::Value = serde_json::from_str(THEMES).unwrap();
        let mut failures = Vec::new();
        let mut selected = Vec::new();
        for theme in file["themes"].as_array().unwrap() {
            let name = theme["name"].as_str().unwrap();
            let highlight = theme["highlight"].as_object().unwrap();
            for key in highlight.keys() {
                assert!(
                    known.get(key).is_some(),
                    "{name}: unknown highlight key {key}"
                );
            }
            let syntax = highlight["syntax"].as_object().unwrap();
            for key in syntax.keys() {
                assert!(
                    known["syntax"].get(key).is_some(),
                    "{name}: unknown syntax key {key}"
                );
            }
            let color = |key: &str| try_parse_color(highlight[key].as_str().unwrap()).unwrap();
            let bg = color("editor.background");
            let mut need = |what: &str, fg: Hsla, min: f32| {
                let ratio = contrast(fg, bg, bg);
                if ratio < min {
                    failures.push(format!("{name}: {what} is {ratio:.2}:1, needs {min}:1"));
                }
            };
            need("editor text", color("editor.foreground"), BODY);
            need("line numbers", color("editor.line_number"), UI);
            // Selected text keeps its syntax color over the selection tint, with every accent.
            let selections: Vec<(&str, Hsla)> = CANDIDATES
                .iter()
                .map(|accent| {
                    let (light, dark) = themes(accent);
                    let config = if name == dark.name.as_ref() {
                        dark
                    } else {
                        light
                    };
                    (accent.name, resolved(config).selection)
                })
                .collect();
            for (key, style) in syntax {
                let fg = try_parse_color(style["color"].as_str().unwrap()).unwrap();
                need(&format!("syntax {key}"), fg, BODY);
                for (accent, selection) in &selections {
                    let ratio = contrast(fg, *selection, bg);
                    if ratio < BODY {
                        selected.push(format!(
                            "{name} / {accent}: selected syntax {key} is {ratio:.2}:1, needs {BODY}:1"
                        ));
                    }
                }
            }
        }
        failures.extend(selected);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// CIELAB (D65) of an opaque color.
    fn lab(color: Hsla) -> [f32; 3] {
        let [r, g, b, _] = rgb(color);
        let lin = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let (r, g, b) = (lin(r), lin(g), lin(b));
        let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
        let f = |t: f32| {
            if t > 0.008856 {
                t.cbrt()
            } else {
                7.787 * t + 16. / 116.
            }
        };
        let (fx, fy, fz) = (f(x), f(y), f(z));
        [116. * fy - 16., 500. * (fx - fy), 200. * (fy - fz)]
    }

    /// The CIEDE2000 color difference: about 1 is just noticeable side by side; small text
    /// needs far more to tell two colors apart at a glance.
    fn delta_e(a: Hsla, b: Hsla) -> f32 {
        use std::f32::consts::PI;
        let ([l1, a1, b1], [l2, a2, b2]) = (lab(a), lab(b));
        let deg = |r: f32| r * 180. / PI;
        let rad = |d: f32| d * PI / 180.;
        let c_bar = (a1.hypot(b1) + a2.hypot(b2)) / 2.;
        let g = 0.5 * (1. - (c_bar.powi(7) / (c_bar.powi(7) + 25f32.powi(7))).sqrt());
        let (a1, a2) = ((1. + g) * a1, (1. + g) * a2);
        let (c1, c2) = (a1.hypot(b1), a2.hypot(b2));
        let hue = |b: f32, a: f32| deg(b.atan2(a)).rem_euclid(360.);
        let (h1, h2) = (hue(b1, a1), hue(b2, a2));
        let dl = l2 - l1;
        let dc = c2 - c1;
        let mut dh = h2 - h1;
        if c1 * c2 == 0. {
            dh = 0.;
        } else if dh > 180. {
            dh -= 360.;
        } else if dh < -180. {
            dh += 360.;
        }
        let dh = 2. * (c1 * c2).sqrt() * (rad(dh) / 2.).sin();
        let l_bar = (l1 + l2) / 2.;
        let c_bar = (c1 + c2) / 2.;
        let h_bar = if c1 * c2 == 0. {
            h1 + h2
        } else if (h1 - h2).abs() <= 180. {
            (h1 + h2) / 2.
        } else if h1 + h2 < 360. {
            (h1 + h2 + 360.) / 2.
        } else {
            (h1 + h2 - 360.) / 2.
        };
        let t = 1. - 0.17 * rad(h_bar - 30.).cos()
            + 0.24 * rad(2. * h_bar).cos()
            + 0.32 * rad(3. * h_bar + 6.).cos()
            - 0.20 * rad(4. * h_bar - 63.).cos();
        let theta = 30. * (-((h_bar - 275.) / 25.).powi(2)).exp();
        let rc = 2. * (c_bar.powi(7) / (c_bar.powi(7) + 25f32.powi(7))).sqrt();
        let sl = 1. + 0.015 * (l_bar - 50.).powi(2) / (20. + (l_bar - 50.).powi(2)).sqrt();
        let sc = 1. + 0.045 * c_bar;
        let sh = 1. + 0.015 * c_bar * t;
        let rt = -rad(2. * theta).sin() * rc;
        ((dl / sl).powi(2) + (dc / sc).powi(2) + (dh / sh).powi(2) + rt * (dc / sc) * (dh / sh))
            .sqrt()
    }

    /// Workload pod prefixes in a log tab must tell pods apart (#62): every pair of prefix
    /// colours differs clearly, in both themes.
    // covers UP-45
    #[test]
    fn log_pod_prefix_colors_tell_pods_apart() {
        // Clearly told apart at a glance in small text (10 is already plainly visible).
        const APART: f32 = 15.;
        let (light, dark) = themes(&ACCENT);
        let mut failures = Vec::new();
        for t in [resolved(light), resolved(dark)] {
            let colors = log_source_colors(&t);
            for (i, a) in colors.iter().enumerate() {
                for (j, b) in colors.iter().enumerate().skip(i + 1) {
                    let difference = delta_e(*a, *b);
                    if difference < APART {
                        failures.push(format!(
                            "{}: prefix colours {i} and {j} differ by {difference:.1}, need {APART}",
                            t.theme_name()
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// A tag's dot and strip are graphics, so they need 3:1 on the sidebar and the detail pane,
    /// and the palette's colours must differ from each other, in both themes.
    // covers UP-83
    #[test]
    fn cluster_tag_colors_are_visible_and_distinct_in_both_themes() {
        use flint_core::settings::ClusterTag;

        const APART: f32 = 15.;
        let (light, dark) = themes(&ACCENT);
        let mut failures = Vec::new();
        for t in [resolved(light), resolved(dark)] {
            let colors = ClusterTag::ALL.map(|tag| (tag, super::cluster_tag_color(tag, &t)));
            for (i, (tag, color)) in colors.iter().enumerate() {
                for surface in [t.sidebar, t.background] {
                    let ratio = contrast(*color, surface, surface);
                    if ratio < 3. {
                        failures.push(format!("{}: {tag:?} at {ratio:.2}:1", t.theme_name()));
                    }
                }
                for (other, second) in &colors[i + 1..] {
                    let difference = delta_e(*color, *second);
                    if difference < APART {
                        failures.push(format!(
                            "{}: {tag:?} and {other:?} differ by {difference:.1}",
                            t.theme_name()
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// How far apart (CIEDE2000) the YAML view's key and value colors must be: enough to tell
    /// `name:` from `name` in small monospace text at a glance.
    const DISTINCT: f32 = 50.;

    /// The YAML view's keys (`property`) and string values must not look alike: in a YAML
    /// document nearly every line is one of each.
    // covers UP-23, UP-32
    #[test]
    fn yaml_keys_and_strings_are_clearly_different_colors() {
        let file: serde_json::Value = serde_json::from_str(THEMES).unwrap();
        let mut failures = Vec::new();
        for theme in file["themes"].as_array().unwrap() {
            let name = theme["name"].as_str().unwrap();
            let syntax = &theme["highlight"]["syntax"];
            let color =
                |key: &str| try_parse_color(syntax[key]["color"].as_str().unwrap()).unwrap();
            let difference = delta_e(color("property"), color("string"));
            if difference < DISTINCT {
                failures.push(format!(
                    "{name}: keys (property) and strings differ by {difference:.1} (CIEDE2000), \
                     need {DISTINCT}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn delta_e_matches_published_ciede2000_values() {
        // Black to white is the full lightness range; a step of 2 in 255 is barely visible.
        let hex = |s: &str| try_parse_color(s).unwrap();
        assert!(delta_e(hex("#2f5d8a"), hex("#2f5d8a")) < 0.01);
        let black_white = delta_e(hex("#000000"), hex("#ffffff"));
        assert!((black_white - 100.).abs() < 0.5, "{black_white}");
        let near = delta_e(hex("#808080"), hex("#828282"));
        assert!(near < 1.5, "{near}");
        // Symmetric.
        let (a, b) = (hex("#1f5fbf"), hex("#266b0e"));
        assert!((delta_e(a, b) - delta_e(b, a)).abs() < 0.01);
    }

    #[test]
    fn contrast_matches_known_wcag_ratios() {
        let hex = |s: &str| try_parse_color(s).unwrap();
        let ratio = contrast(hex("#000000"), hex("#ffffff"), hex("#ffffff"));
        assert!((ratio - 21.).abs() < 0.01, "{ratio}");
        let ratio = contrast(hex("#767676"), hex("#ffffff"), hex("#ffffff"));
        assert!((ratio - 4.54).abs() < 0.01, "{ratio}");
        // Half-transparent black over white is mid gray.
        let ratio = contrast(hex("#00000080"), hex("#ffffff"), hex("#ffffff"));
        assert!((ratio - 3.95).abs() < 0.1, "{ratio}");
    }
}
