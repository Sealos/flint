//! View test for the appearance setting: the main window on a real core, switched between
//! System, Light and Dark by dispatching the View > Appearance menu actions, with the production
//! handlers installed.

use std::time::Duration;

use flint_core::{Appearance, CoreConfig, CoreHandle, Settings};
use gpui_kit::component::{ActiveTheme as _, ThemeMode, try_parse_color};
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, TestAppContext, Window,
    WindowBounds, WindowOptions, px, size,
};

use super::accent::ACCENT;
use super::clusters::ClustersModel;
use super::registry::{AppearanceDark, AppearanceLight, AppearanceSystem};
use super::theme;
use super::ui_state::UiState;
use super::workspace::Workspace;

const WAIT: Duration = Duration::from_secs(10);

fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<ClustersModel>, tempfile::TempDir) {
    cx.executor().allow_parking();
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
    let config_dir = dir.path().to_owned();
    let (window, model) = cx.update(|cx| {
        gpui_kit::init(cx);
        theme::init(ThemeMode::Light, cx);
        super::registry::init(cx);
        let model = cx.new(|cx| ClustersModel::new(core, Settings::default(), None, cx));
        super::install_menus(Appearance::System, cx);
        super::handle_appearance_menu(&model, cx);
        let ws_model = model.clone();
        let window = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1000.), px(600.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    Workspace::new(ws_model, UiState::load(config_dir), None, None, window, cx)
                })
            },
        )
        .unwrap();
        (window.0, model)
    });
    (window, model, dir)
}

/// Whether the window shows the theme `name`, in `mode`, with the background that theme's data
/// sets.
fn shows(name: &'static str, mode: ThemeMode) -> impl FnMut(&mut Window, &mut App) -> bool {
    let (light, dark) = theme::themes(&ACCENT);
    let data = if mode.is_dark() { dark } else { light };
    let background = hex(data.colors.background.as_deref().unwrap());
    move |_, cx| {
        let theme = cx.theme();
        theme.mode == mode && theme.theme_name() == name && theme.background == background
    }
}

/// The View > Appearance item that has the check mark.
fn checked(cx: &mut TestAppContext) -> Vec<String> {
    use gpui_kit::OwnedMenuItem;
    let menus = cx.update(|cx| cx.get_menus()).unwrap();
    let view = menus.iter().find(|menu| menu.name == "View").unwrap();
    let Some(appearance) = view.items.iter().find_map(|item| match item {
        OwnedMenuItem::Submenu(menu) if menu.name == "Appearance" => Some(menu),
        _ => None,
    }) else {
        panic!("View has no Appearance submenu");
    };
    appearance
        .items
        .iter()
        .filter_map(|item| match item {
            OwnedMenuItem::Action {
                name,
                checked: true,
                ..
            } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

fn hex(color: &str) -> gpui_kit::Hsla {
    try_parse_color(color).unwrap()
}

// covers UP-32 (UP-13, following macOS live, needs an appearance change the test platform lacks)
#[gpui_kit::test]
async fn switching_appearance_swaps_the_theme_colors(cx: &mut TestAppContext) {
    let (window, _model, dir) = open(cx);
    // `system` follows the window's appearance, which is light on the test platform.
    cx.wait_for(window, WAIT, shows("Flint Light", ThemeMode::Light))
        .await;
    let light_text = cx.update(|cx| cx.theme().foreground);
    assert_eq!(checked(cx), ["Use System Appearance"]);

    cx.update(|cx| cx.dispatch_action(&AppearanceDark));
    cx.wait_for(window, WAIT, shows("Flint Dark", ThemeMode::Dark))
        .await;
    cx.update(|cx| {
        let theme = cx.theme();
        assert_ne!(theme.foreground, light_text);
        assert_eq!(theme.primary, hex(ACCENT.dark.fill));
    });
    assert_eq!(checked(cx), ["Use Dark Appearance"]);
    // Persisted: written to settings.toml, so the next start opens dark.
    let saved = flint_core::settings::load_settings(dir.path()).unwrap();
    assert_eq!(saved.appearance, Appearance::Dark);

    cx.update(|cx| cx.dispatch_action(&AppearanceLight));
    cx.wait_for(window, WAIT, shows("Flint Light", ThemeMode::Light))
        .await;
    cx.update(|cx| {
        assert_eq!(cx.theme().foreground, light_text);
        assert_eq!(cx.theme().primary, hex(ACCENT.light.fill));
    });
    assert_eq!(checked(cx), ["Use Light Appearance"]);

    // The same action by name, as the palette and automation send it.
    cx.update_window(window, |_, window, cx| {
        super::registry::dispatch("flint::AppearanceDark", serde_json::Value::Null, window, cx)
    })
    .unwrap()
    .unwrap();
    cx.wait_for(window, WAIT, shows("Flint Dark", ThemeMode::Dark))
        .await;
    // Back to following the window's own appearance.
    cx.update(|cx| cx.dispatch_action(&AppearanceSystem));
    cx.wait_for(window, WAIT, shows("Flint Light", ThemeMode::Light))
        .await;
    assert_eq!(checked(cx), ["Use System Appearance"]);
}

// covers UP-06: switching the theme keeps the sidebar's notice (the last error) on screen.
#[gpui_kit::test]
async fn switching_appearance_keeps_the_last_error(cx: &mut TestAppContext) {
    let (window, model, _dir) = open(cx);
    cx.wait_for(window, WAIT, shows("Flint Light", ThemeMode::Light))
        .await;
    cx.update(|cx| {
        model.update(cx, |m, _| {
            m.last_error = Some("\"gone\" was removed from your kubeconfig files.".into())
        })
    });
    cx.update(|cx| cx.dispatch_action(&AppearanceDark));
    cx.wait_for(window, WAIT, shows("Flint Dark", ThemeMode::Dark))
        .await;
    let error = cx.update(|cx| model.read(cx).last_error.clone());
    assert_eq!(
        error.as_deref(),
        Some("\"gone\" was removed from your kubeconfig files.")
    );
}
