//! The GPUI front end. Views read from [`clusters::ClustersModel`], which is fed by the core's
//! event stream; the UI never waits on core work (ADR-0002).
//!
//! Views take every color from theme tokens (`cx.theme()`), never a hard-coded color
//! (ARCHITECTURE.md, "Theme tokens only"). The tokens come from Flint Light and Flint Dark
//! ([`theme`]).

mod accent;
mod activity_view;
mod alert_center;
mod app_menus;
#[cfg(test)]
mod app_menus_tests;
#[cfg(test)]
mod appearance_tests;
#[cfg(feature = "automation")]
mod automation;
mod closing;
#[cfg(test)]
mod closing_tests;
mod cluster_name;
mod cluster_tag;
mod clusters;
mod compare;
mod confirm;
mod connection_progress;
mod dashboard;
#[cfg(test)]
#[path = "dashboard_wiring_tests.rs"]
mod dashboard_wiring_tests;
mod detail;
mod dock;
mod dock_model;
#[cfg(test)]
mod dock_tests;
mod fleet_plan;
mod fleet_view;
mod focus;
mod helm_view;
mod icons;
mod keep_watching;
mod list;
mod list_model;
#[cfg(test)]
mod list_sort_tests;
mod list_usage;
mod log_model;
mod log_view;
mod menu_fit;
mod menu_record;
mod narrow_text;
mod nav;
mod notice;
mod object;
mod palette;
mod palette_model;
mod pending_threshold;
mod port_forward_view;
mod quit_flush;
mod registry;
mod resource_actions;
mod row_menu;
mod scope_picker;
mod set_kinds;
mod set_members;
mod settings_draft;
mod settings_thresholds;
#[cfg(test)]
mod settings_thresholds_tests;
mod settings_view;
#[cfg(test)]
mod settings_view_tests;
mod startup;
#[cfg(test)]
mod support_tests;
mod switcher_model;
mod terminal_keys;
mod terminal_model;
mod terminal_palette;
mod terminal_view;
mod theme;
mod tooltip_text;
mod ui_state;
#[cfg(any(test, feature = "automation"))]
mod view_state;
mod workspace;
mod write_binding;
mod write_confirm;
mod yaml_diff;

use std::cell::Cell;
use std::rc::Rc;

use flint_core::bootstrap::Bootstrap;
use flint_core::{Appearance, CoreHandle};
use gpui_kit::component::ThemeMode;
use gpui_kit::*;

use clusters::ClustersModel;
use gpui_kit::component::input;
use registry::{
    About, AppearanceDark, AppearanceLight, AppearanceSystem, GoBack, Hide, HideOthers,
    MinimizeWindow, OpenHelp, OpenSettings, Quit, ShowAll, ToggleCommandPalette,
    ToggleResourceSwitcher, ZoomWindow,
};
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
            flint_core::bootstrap::flush_logs();
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
    let settings_dir = boot.config_dir.clone();
    // A development and testing aid, for screenshots of both themes: `light` or `dark`, over the
    // `appearance` setting and macOS.
    let forced_appearance = match std::env::var("FLINT_APPEARANCE").as_deref() {
        Ok("light") => Some(ThemeMode::Light),
        Ok("dark") => Some(ThemeMode::Dark),
        _ => None,
    };

    let settings = boot.settings.clone();
    gpui_kit::application()
        .with_assets(icons::FlintAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.set_global(workspace::SettingsDir(settings_dir.clone()));
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
            // On macOS a quit ends the process without returning from `run`.
            let quitting = core.clone();
            cx.on_app_quit(move |cx| {
                let flush = quit_flush::QuitFlush::join(cx);
                // Background sign-ins end first: nothing else would stop them on the way out.
                let plugins = quitting.stop_keep_watching();
                async move {
                    plugins.await;
                    flush.finish().await;
                }
            })
            .detach();
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let model = cx.new(|cx| ClustersModel::new(core, settings, startup_cluster, cx));
            handle_appearance_menu(&model, cx);
            alert_center::install(&model, cx);
            let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
            match gpui_kit::open_window(window_options(bounds), cx, |window, cx| {
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

/// The main window's options, opening at `bounds`. It cannot be made smaller than
/// [`workspace::MIN_WINDOW_SIZE`], the smallest size the layout works at.
fn window_options(bounds: Bounds<Pixels>) -> WindowOptions {
    let (width, height) = workspace::MIN_WINDOW_SIZE;
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(width), px(height))),
        titlebar: Some(TitlebarOptions {
            // Still the window's name in Mission Control and the Window menu. On macOS the
            // panes reach the window's top edge, the traffic lights centered on their title row
            // ([`theme::traffic_light_inset`]).
            title: Some("Flint".into()),
            appears_transparent: cfg!(target_os = "macos"),
            traffic_light_position: cfg!(target_os = "macos")
                .then(|| gpui_kit::point(px(12.), px(9.))),
        }),
        // The header's controls sit where the title bar was, so AppKit must not take their
        // clicks: the empty parts of those rows move the window ([`theme::moves_window`]).
        app_owns_titlebar_drag: cfg!(target_os = "macos"),
        // Linux desktops match the window to `flint.desktop` (packaging/linux) by this id
        // (Wayland app_id, X11 WM_CLASS), for its icon and taskbar grouping.
        app_id: Some("flint".into()),
        ..Default::default()
    }
}

/// The menus (see [`all_menus`]), every item a registered action (their keys come from the registry)
/// except Services, which the OS fills: the app menu (About Flint, Settings…, Services, Hide Flint, Hide
/// Others, Show All, Quit), Edit (Undo, Redo, Cut, Copy, Paste and Select All, for the text
/// fields), View (the command palette, Appearance), File, Cluster and Resource ([`app_menus`]),
/// Window (Minimize, Zoom) and Help. Quit and Close Window ask first while a session would end ([`closing`]).
/// GPUI shows no menu bar on Linux: there, every item is in the command palette (Ctrl-Shift-P),
/// and Quit and Close Window have their keys too.
fn install_menus(appearance: Appearance, cx: &mut App) {
    closing::install(cx);
    // Hiding is the app's, not a window's.
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &OpenHelp, cx| cx.open_url(env!("CARGO_PKG_REPOSITORY")));
    base::Root::register_plugin(cx, |_, _| WindowActions);
    set_menus(appearance, cx);
}

/// The actions that need a window. They are handled around each window's whole content, dialogs
/// included, and not globally, so the native menu disables them while no window is active.
struct WindowActions;

impl Render for WindowActions {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl base::RootPlugin for WindowActions {
    fn decorate(
        &self,
        surface: AnyElement,
        _: &base::Root,
        _: &mut Window,
        _: &mut App,
    ) -> impl IntoElement {
        div()
            .size_full()
            .on_action(|_: &MinimizeWindow, window, _| window.minimize_window())
            .on_action(|_: &ZoomWindow, window, _| window.zoom_window())
            .child(surface)
    }
}

fn set_menus(appearance: Appearance, cx: &mut App) {
    app_menus::update(cx, |inputs| inputs.appearance = appearance);
}

/// The whole menu bar: the standard menus around File, Cluster and Resource ([`app_menus`]).
fn all_menus(inputs: &app_menus::MenuInputs, enabled: app_menus::Enabled) -> Vec<Menu> {
    vec![
        Menu::new("Flint").items([
            MenuItem::action(registry::label::<About>(), About),
            MenuItem::separator(),
            // "…": it opens a view of its own, as macOS apps write it.
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(registry::label::<Hide>(), Hide),
            MenuItem::action(registry::label::<HideOthers>(), HideOthers),
            MenuItem::action(registry::label::<ShowAll>(), ShowAll),
            MenuItem::separator(),
            MenuItem::action(registry::label::<Quit>(), Quit),
        ]),
        app_menus::file_menu(enabled),
        edit_menu(),
        view_menu(inputs.appearance),
        app_menus::cluster_menu(inputs, enabled),
        app_menus::resource_menu(enabled),
        Menu::new("Window").items([
            MenuItem::action(registry::label::<MinimizeWindow>(), MinimizeWindow),
            MenuItem::action(registry::label::<ZoomWindow>(), ZoomWindow),
        ]),
        Menu::new("Help").items([MenuItem::action(registry::label::<OpenHelp>(), OpenHelp)]),
    ]
}

/// Edit: Undo, Redo, Cut, Copy, Paste and Select All, which the focused text field handles. A
/// shell tab keeps its own copy and paste: its keys reach it first, and these do nothing there.
fn edit_menu() -> Menu {
    Menu::new("Edit").items([
        MenuItem::os_action(
            registry::label::<input::Undo>(),
            input::Undo,
            OsAction::Undo,
        ),
        MenuItem::os_action(
            registry::label::<input::Redo>(),
            input::Redo,
            OsAction::Redo,
        ),
        MenuItem::separator(),
        MenuItem::os_action(registry::label::<input::Cut>(), input::Cut, OsAction::Cut),
        MenuItem::os_action(
            registry::label::<input::Copy>(),
            input::Copy,
            OsAction::Copy,
        ),
        MenuItem::os_action(
            registry::label::<input::Paste>(),
            input::Paste,
            OsAction::Paste,
        ),
        MenuItem::os_action(
            registry::label::<input::SelectAll>(),
            input::SelectAll,
            OsAction::SelectAll,
        ),
    ])
}

/// View: Show Command Palette, Go to Resource, Go Back, and Appearance.
fn view_menu(appearance: Appearance) -> Menu {
    Menu::new("View").items([
        MenuItem::action(
            registry::label::<ToggleCommandPalette>(),
            ToggleCommandPalette,
        ),
        MenuItem::action(
            registry::label::<ToggleResourceSwitcher>(),
            ToggleResourceSwitcher,
        ),
        MenuItem::action(registry::label::<GoBack>(), GoBack),
        MenuItem::separator(),
        MenuItem::submenu(appearance_menu(appearance)),
    ])
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
    Menu::new("Appearance").items([
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
    ])
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
