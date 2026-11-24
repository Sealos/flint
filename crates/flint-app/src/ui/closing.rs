//! Closing the window and quitting (#177). Both end every session in the dock, so while a shell
//! or a following log tab is still at work they ask first, naming how many sessions end. Close
//! Window (its key, the Window menu, Cmd-W with no dock tab shown), the window's own close button
//! and Quit all take this path; with nothing at work they close or quit at once.
//!
//! Quitting from outside the app (the Dock icon's Quit, logging out) is not asked about: GPUI
//! offers no hook that can cancel it.

// module-tests: closing_tests.rs

use gpui_kit::base::Root;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;

use super::confirm::{self, DialogText};
use super::dock_model::LiveSessions;
use super::registry::{CloseWindow, Quit};
use super::workspace::Workspace;

/// What ends the sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Closing the window, which ends its dock's sessions.
    Window,
    /// Quitting, which ends every window's.
    App,
}

/// Handles Quit and Close Window, app-wide: they also come from the menu bar, whatever has focus.
pub fn install(cx: &mut App) {
    // Both are dispatched through the front window, which cannot be updated until that
    // dispatch ends: act right after.
    cx.on_action(|_: &Quit, cx| cx.defer(quit));
    cx.on_action(|_: &CloseWindow, cx| {
        // The front window, or the first one while the app is not in front (macOS may refuse to
        // bring it forward, and a menu item or key reaches the app only once it is).
        if let Some(window) = cx.active_window().or_else(|| cx.windows().first().copied()) {
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| close_window(window, cx));
            });
        }
    });
}

/// Makes the window's close button ask first, as Close Window does. Call once per window.
pub fn guard_close_button(window: &Window, cx: &App) {
    window.on_window_should_close(cx, |window, cx| {
        let live = live_sessions(window, cx);
        if live.is_empty() {
            return true;
        }
        ask(window, cx, Ending::Window, live);
        false
    });
}

/// Closes `window`, asking first while a session in it would end.
fn close_window(window: &mut Window, cx: &mut App) {
    let live = live_sessions(window, cx);
    if live.is_empty() {
        window.remove_window();
    } else {
        ask(window, cx, Ending::Window, live);
    }
}

/// Quits, asking first (over the front window) while a session in any window would end.
fn quit(cx: &mut App) {
    let mut live = LiveSessions::default();
    for handle in cx.windows() {
        if let Ok(more) = handle.update(cx, |_, window, cx| live_sessions(window, cx)) {
            live += more;
        }
    }
    let front = cx.active_window().or_else(|| cx.windows().first().copied());
    let asked = match front {
        Some(front) if !live.is_empty() => front
            .update(cx, |_, window, cx| ask(window, cx, Ending::App, live))
            .is_ok(),
        _ => false,
    };
    if !asked {
        quit_now(cx);
    }
}

/// Opens the confirmation over `window`; confirming closes the window, or quits. Not while a
/// dialog is open there: pressing Cmd-Q or the close button again opens no second one.
fn ask(window: &mut Window, cx: &mut App, ending: Ending, live: LiveSessions) {
    if window.has_active_dialog(cx) {
        return;
    }
    confirm::open(
        window,
        cx,
        dialog(ending, live),
        move |window, cx| match ending {
            Ending::Window => window.remove_window(),
            Ending::App => quit_now(cx),
        },
    );
}

/// The sessions at work in `window`'s workspace, if it holds one.
fn live_sessions(window: &Window, cx: &App) -> LiveSessions {
    window
        .root::<Root>()
        .flatten()
        .and_then(|root| root.read(cx).view().clone().downcast::<Workspace>().ok())
        .map(|workspace| workspace.read(cx).live_sessions(cx))
        .unwrap_or_default()
}

/// The confirmation for `ending` while `live` sessions are at work.
pub fn dialog(ending: Ending, live: LiveSessions) -> DialogText {
    let (title, doing, ok, id) = match ending {
        Ending::Window => (
            "Close the window?",
            "Closing the window",
            "Close Window",
            "close-window-description",
        ),
        Ending::App => ("Quit Flint?", "Quitting", "Quit", "quit-description"),
    };
    let (sessions, output) = match live.total() {
        1 => ("1 session".to_owned(), "Its"),
        n => (format!("{n} sessions"), "Their"),
    };
    DialogText {
        title: title.into(),
        text: format!(
            "{doing} ends {sessions} still running in the dock: {}. {output} output is not kept.",
            live.described()
        ),
        ok: ok.into(),
        cancel: "Cancel".into(),
        id,
        // Ending sessions takes the button: Enter cancels.
        default_cancel: true,
    }
}

/// Quits now. Test builds also count it: the test platform's quit does nothing.
fn quit_now(cx: &mut App) {
    #[cfg(test)]
    {
        cx.default_global::<Quits>().0 += 1;
    }
    cx.quit();
}

/// How many times the app quit, in view tests.
#[cfg(test)]
#[derive(Default)]
pub struct Quits(pub usize);

#[cfg(test)]
impl Global for Quits {}
