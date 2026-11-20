//! The GPUI front end. Views read from [`clusters::ClustersModel`], which is fed by the core's
//! event stream; the UI never waits on core work (ADR-0002).
//!
//! Views take every color from theme tokens (`cx.theme()`), never a hard-coded color
//! (ARCHITECTURE.md, "Theme tokens only"). The tokens come from Flint Light and Flint Dark
//! ([`theme`]).

mod accent;
#[cfg(test)]
mod appearance_tests;
#[cfg(feature = "automation")]
mod automation;
mod clusters;
mod detail;
mod dock;
mod dock_model;
#[cfg(test)]
mod dock_tests;
mod list;
mod list_model;
mod log_model;
mod log_view;
mod nav;
mod notice;
mod object;
mod registry;
mod row_menu;
mod startup;
#[cfg(test)]
mod support_tests;
mod terminal_keys;
mod terminal_model;
mod terminal_palette;
mod terminal_view;
mod theme;
mod ui_state;
#[cfg(any(test, feature = "automation"))]
mod view_state;
mod workspace;

use std::cell::Cell;
use std::rc::Rc;

use flint_core::bootstrap::Bootstrap;
use flint_core::{Appearance, CoreHandle};
use gpui_kit::component::ThemeMode;
use gpui_kit::*;

use clusters::ClustersModel;
use registry::{AppearanceDark, AppearanceLight, AppearanceSystem, CloseWindow, Quit};
use ui_state::UiState;
use workspace::Workspace;

/// Names a context to activate at startup, exactly as clicking it would. A development and testing
/// aid (for example for agents taking screenshots); it never selects anything else.
const FLINT_CLUSTER: &str = "FLINT_CLUSTER";

pub fn run(boot: Bootstrap) {
    // Before the core starts: a test build refuses unsafe places, and exits instead.
    #[cfg(feature = "automation")]
    let automation = automation::prepare();
    let core = match CoreHandle::start(boot.core_config()) {
        Ok(core) => core,
        Err(e) => {
            tracing::error!("could not start the core runtime: {e}");
            std::process::exit(1);
        }
    };
    let startup_cluster = match std::env::var_os(FLINT_CLUSTER) {
        None => None,
        Some(value) => match value.into_string() {
            Ok(name) if !name.is_empty() => Some(name),
            Ok(_) => {
                tracing::warn!("FLINT_CLUSTER is empty; starting with nothing selected");
                None
            }
            Err(_) => {
                tracing::warn!("FLINT_CLUSTER is not UTF-8; starting with nothing selected");
                None
            }
        },
    };

    let startup_view = match std::env::var(startup::FLINT_VIEW) {
        Ok(value) if !value.is_empty() => match startup::parse(&value) {
            Some(view) if startup_cluster.is_some() => Some(view),
            Some(_) => {
                tracing::warn!("FLINT_VIEW is ignored without FLINT_CLUSTER");
                None
            }
            None => {
                tracing::warn!(
                    value,
                    "FLINT_VIEW is not kind, kind/name or namespace/kind/name"
                );
                None
            }
        },
        _ => None,
    };
    let ui_state = UiState::load(boot.config_dir.clone());
    // A development and testing aid, for screenshots of both themes: `light` or `dark`, over the
    // `appearance` setting and macOS.
    let forced_appearance = match std::env::var("FLINT_APPEARANCE").as_deref() {
        Ok("light") => Some(ThemeMode::Light),
        Ok("dark") => Some(ThemeMode::Dark),
        _ => None,
    };

    let settings = boot.settings.clone();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // The window is not open yet: start from the app's appearance; the workspace
            // follows the window's from then on.
            theme::init(
                theme::resolve(
                    settings.appearance,
                    forced_appearance,
                    cx.window_appearance(),
                ),
                cx,
            );
            registry::init(cx);
            install_menus(settings.appearance, cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let model = cx.new(|cx| ClustersModel::new(core, settings, startup_cluster, cx));
            handle_appearance_menu(&model, cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(800.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Flint".into()),
                    ..Default::default()
                }),
                // Linux desktops match the window to `flint.desktop` (packaging/linux) by this
                // id (Wayland app_id, X11 WM_CLASS), for its icon and taskbar grouping.
                app_id: Some("flint".into()),
                ..Default::default()
            };
            match gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    Workspace::new(model, ui_state, startup_view, forced_appearance, window, cx)
                })
            }) {
                #[cfg(feature = "automation")]
                Ok((window, workspace)) => {
                    automation::start(automation.clone(), window, workspace, cx)
                }
                #[cfg(not(feature = "automation"))]
                Ok(_) => {}
                Err(e) => {
                    tracing::error!("could not open the window: {e:#}");
                    cx.quit();
                }
            }
            cx.activate(true);
        });
}

/// The app menu with Quit and Close Window (their keys come from the registry), plus
/// View > Appearance. GPUI shows no menu bar on Linux: there, Quit and Close Window are reached
/// by their keys, and Appearance by the `appearance` setting until the command palette (#29).
fn install_menus(appearance: Appearance, cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| {
        // The action is dispatched through the front window, which cannot be updated until
        // that dispatch ends: close it right after.
        if let Some(window) = cx.active_window() {
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, _| window.remove_window());
            });
        }
    });
    set_menus(appearance, cx);
}

fn set_menus(appearance: Appearance, cx: &mut App) {
    cx.set_menus([
        Menu::new("Flint").items([MenuItem::action(registry::label::<Quit>(), Quit)]),
        appearance_menu(appearance),
        Menu::new("Window").items([MenuItem::action(
            registry::label::<CloseWindow>(),
            CloseWindow,
        )]),
    ]);
}

/// View > Appearance: System, Light and Dark, with a check mark on the current setting.
fn appearance_menu(appearance: Appearance) -> Menu {
    let item = |name: &'static str, action: Box<dyn Action>, this: Appearance| MenuItem::Action {
        name: name.into(),
        action,
        os_action: None,
        checked: appearance == this,
        disabled: false,
    };
    Menu::new("View").items([MenuItem::submenu(Menu::new("Appearance").items([
        item(
            registry::label::<AppearanceSystem>(),
            Box::new(AppearanceSystem),
            Appearance::System,
        ),
        item(
            registry::label::<AppearanceLight>(),
            Box::new(AppearanceLight),
            Appearance::Light,
        ),
        item(
            registry::label::<AppearanceDark>(),
            Box::new(AppearanceDark),
            Appearance::Dark,
        ),
    ]))])
}

/// Handles the View > Appearance actions by writing the `appearance` setting, and moves the
/// menu's check mark when the setting changes (from the menu or from `settings.toml`).
fn handle_appearance_menu(model: &Entity<ClustersModel>, cx: &mut App) {
    fn set(model: &Entity<ClustersModel>, appearance: Appearance, cx: &mut App) {
        model.update(cx, |m, _| m.set_appearance(appearance));
    }
    let m = model.clone();
    cx.on_action(move |_: &AppearanceSystem, cx| set(&m, Appearance::System, cx));
    let m = model.clone();
    cx.on_action(move |_: &AppearanceLight, cx| set(&m, Appearance::Light, cx));
    let m = model.clone();
    cx.on_action(move |_: &AppearanceDark, cx| set(&m, Appearance::Dark, cx));
    let shown = Rc::new(Cell::new(model.read(cx).settings.appearance));
    cx.observe(model, move |model, cx| {
        let appearance = model.read(cx).settings.appearance;
        if shown.replace(appearance) != appearance {
            set_menus(appearance, cx);
        }
    })
    .detach();
}
