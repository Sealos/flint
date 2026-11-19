//! Flint Light and Flint Dark, and which of them is shown (ARCHITECTURE.md, "Look and keyboard").
//!
//! The themes are gpui-component theme data (`themes/flint.json`, embedded at build time) with
//! the orange accent from [`super::accent`] applied on top. Views never name a color: they read
//! theme tokens (`cx.theme()`), so this module and the theme data are the only places colors live
//! (`scripts/check-theme-colors.sh` enforces it).

use std::rc::Rc;

use flint_core::Appearance;
use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode, ThemeSet};
use gpui_kit::{App, SharedString, WindowAppearance};

use super::accent::{ACCENT, Accent, Shades};

/// Both themes, without the accent.
const THEMES: &str = include_str!("themes/flint.json");

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
    // Selection: a tint behind normal text. The `*_active_border` tokens are for components
    // that draw an edge; Flint's own views draw only the tint.
    colors.selection = a(shades.fill, "4d");
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
mod tests {
    // Not `super::*`: that brings in `gpui_kit::test`, which shadows the std `#[test]`.
    use super::{THEMES, chrome, resolve, themes};
    use crate::ui::accent::{ACCENT, CANDIDATES};
    use flint_core::Appearance;
    use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode, try_parse_color};
    use gpui_kit::{Hsla, Rgba, WindowAppearance};
    use std::rc::Rc;

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
    fn resolved(config: ThemeConfig) -> Theme {
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
    fn contrast(fg: Hsla, bg: Hsla, base: Hsla) -> f32 {
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
        need(
            "connected mark on the active cluster",
            t.success,
            active_cluster,
            base,
            UI,
        );
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
            for (key, style) in syntax {
                let fg = try_parse_color(style["color"].as_str().unwrap()).unwrap();
                need(&format!("syntax {key}"), fg, BODY);
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
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
