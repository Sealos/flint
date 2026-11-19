//! The GPUI front end. Views read from [`clusters::ClustersModel`], which is fed by the core's
//! event stream; the UI never waits on core work (ADR-0002).
//!
//! Views take every color from theme tokens (`cx.theme()`), never a hard-coded color, so light and
//! dark mode both work and the M1.5 themes need no view changes (ARCHITECTURE.md, "Theme tokens
//! only").

mod clusters;
mod display;
mod workspace;

use flint_core::CoreHandle;
use flint_core::bootstrap::Bootstrap;
use gpui_kit::*;

use clusters::ClustersModel;
use workspace::Workspace;

actions!(flint, [Quit, CloseWindow]);

/// Names a context to activate at startup, exactly as clicking it would. A development and testing
/// aid (for example for agents taking screenshots); it never selects anything else.
const FLINT_CLUSTER: &str = "FLINT_CLUSTER";

pub fn run(boot: Bootstrap) {
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

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            install_menus(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let model = cx.new(|cx| ClustersModel::new(core, startup_cluster, cx));
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(720.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Flint".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(model, window, cx))
            }) {
                tracing::error!("could not open the window: {e:#}");
                cx.quit();
            }
            cx.activate(true);
        });
}

/// The app menu with Quit (cmd-q) and Close Window (cmd-w).
fn install_menus(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    });
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
    ]);
    cx.set_menus([
        Menu::new("Flint").items([MenuItem::action("Quit Flint", Quit)]),
        Menu::new("Window").items([MenuItem::action("Close Window", CloseWindow)]),
    ]);
}
