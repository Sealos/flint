//! Closing the window and quitting (#177). Both end every session in the dock and stop every
//! running port forward, so while a shell, a following log tab or a forward is still at work they
//! ask first, naming how many end. Close Window (its key, the File menu, Cmd-W with no dock tab
//! shown), the window's own close button and Quit all take this path; with nothing at work they
//! close or quit at once. An unapplied YAML edit on top of that joins the same question, so it is
//! one prompt, never two. Unsaved edits in Settings are asked about first, on their own: Save,
//! Discard or Cancel; Save and Discard go on to close or quit, asking the rest then.
//!
//! Quitting from outside the app (the Dock icon's Quit, logging out) is not asked about, and
//! loses an unapplied YAML edit: GPUI offers no hook that can cancel it.

// module-tests: closing_tests.rs

use gpui_kit::base::Root;
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
    window.on_window_should_close(cx, closes_now);
}

/// Closes `window`, asking first while a session in it would end.
fn close_window(window: &mut Window, cx: &mut App) {
    if closes_now(window, cx) {
        window.remove_window();
    }
}

/// Whether `window` may close now. If not, a question is open over it (or one already was).
fn closes_now(window: &mut Window, cx: &mut App) -> bool {
    if asks_about_settings(window, cx, Ending::Window, close_window_after) {
        return false;
    }
    let losses = Losses::of(window, cx);
    if losses.edits > 0 && losses.ends_more() {
        ask(window, cx, Ending::Window, losses);
        return false;
    }
    if asks_about_edit(window, cx, close_window_after) {
        return false;
    }
    if losses.ends_more() {
        ask(window, cx, Ending::Window, losses);
        return false;
    }
    true
}

/// Quits, asking first (over the front window) while a session in any window would end.
fn quit(cx: &mut App) {
    // Unsaved settings first, over the window that holds them: Save or Discard come back here.
    for handle in cx.windows() {
        let asked = handle.update(cx, |_, window, cx| {
            let held = workspace(window, cx).is_some_and(|ws| ws.read(cx).has_unsaved_settings(cx));
            if held {
                window.activate_window();
            }
            held && asks_about_settings(window, cx, Ending::App, |_, cx| cx.defer(quit))
        });
        if matches!(asked, Ok(true)) {
            return;
        }
    }
    let mut losses = Losses::default();
    for handle in cx.windows() {
        if let Ok(more) = handle.update(cx, |_, window, cx| Losses::of(window, cx)) {
            losses += more;
        }
    }
    let front = cx.active_window().or_else(|| cx.windows().first().copied());
    // An edit with more to lose: one question for all of it.
    if losses.edits > 0
        && losses.ends_more()
        && let Some(front) = front
    {
        let _ = front.update(cx, |_, window, cx| {
            window.activate_window();
            ask(window, cx, Ending::App, losses);
        });
        return;
    }
    // Each window's edit in turn, over that window: Discard comes back here for the next.
    for handle in cx.windows() {
        let asked = handle.update(cx, |_, window, cx| {
            let held =
                workspace(window, cx).is_some_and(|ws| ws.read(cx).unapplied_edit(cx).is_some());
            if held {
                window.activate_window();
            }
            held && asks_about_edit(window, cx, |_, cx| cx.defer(quit))
        });
        if matches!(asked, Ok(true)) {
            return;
        }
    }
    let asked = match front {
        Some(front) if losses.ends_more() => front
            .update(cx, |_, window, cx| ask(window, cx, Ending::App, losses))
            .is_ok(),
        _ => false,
    };
    if !asked {
        quit_now(cx);
    }
}

/// Opens the confirmation over `window`; confirming closes the window, or quits. Not while a
/// confirmation is open there (Settings may be): pressing Cmd-Q or the close button again opens
/// no second one.
fn ask(window: &mut Window, cx: &mut App, ending: Ending, losses: Losses) {
    if confirm::any_open(window, cx) {
        return;
    }
    confirm::open(
        window,
        cx,
        dialog(ending, losses),
        move |window, cx| match ending {
            Ending::Window => window.remove_window(),
            Ending::App => quit_now(cx),
        },
    );
}

/// Asks about an unapplied YAML edit in `window`, if it holds one; Discard runs `then`. Whether
/// the caller must stop.
fn asks_about_edit(
    window: &mut Window,
    cx: &mut App,
    then: impl Fn(&mut Window, &mut App) + 'static,
) -> bool {
    let Some(workspace) = workspace(window, cx) else {
        return false;
    };
    workspace.update(cx, |ws, cx| {
        ws.asks_before_leaving(window, cx, move |_, window, cx| then(window, cx))
    })
}

/// Asks Save / Discard / Cancel about unsaved settings in `window`, if it holds any; Save (once
/// written) and Discard run `then`. Whether the caller must stop.
fn asks_about_settings(
    window: &mut Window,
    cx: &mut App,
    ending: Ending,
    then: impl Fn(&mut Window, &mut App) + 'static,
) -> bool {
    let Some(workspace) = workspace(window, cx) else {
        return false;
    };
    let doing = match ending {
        Ending::Window => "Closing the window",
        Ending::App => "Quitting",
    };
    workspace.update(cx, |ws, cx| ws.asks_about_settings(window, cx, doing, then))
}

/// Runs Close Window again once the update under way has ended.
fn close_window_after(window: &mut Window, cx: &mut App) {
    let handle = window.window_handle();
    cx.defer(move |cx| {
        let _ = handle.update(cx, |_, window, cx| close_window(window, cx));
    });
}

fn workspace(window: &Window, cx: &App) -> Option<Entity<Workspace>> {
    window
        .root::<Root>()
        .flatten()
        .and_then(|root| root.read(cx).view().clone().downcast::<Workspace>().ok())
}

/// What closing a window or quitting would lose beyond a plain exit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Losses {
    /// Shells and following log tabs at work.
    pub live: LiveSessions,
    /// Port forwards running.
    pub forwards: usize,
    /// Unapplied YAML edits.
    pub edits: usize,
}

impl Losses {
    /// What closing `window` would lose, if it holds a workspace.
    fn of(window: &Window, cx: &App) -> Self {
        workspace(window, cx).map_or_else(Self::default, |ws| {
            let ws = ws.read(cx);
            Self {
                live: ws.live_sessions(cx),
                forwards: ws.running_forwards(cx),
                edits: usize::from(ws.unapplied_edit(cx).is_some()),
            }
        })
    }

    /// Sessions or forwards would end: what needs the question when no edit asks it.
    fn ends_more(self) -> bool {
        !self.live.is_empty() || self.forwards > 0
    }
}

impl std::ops::AddAssign for Losses {
    fn add_assign(&mut self, other: Self) {
        self.live += other.live;
        self.forwards += other.forwards;
        self.edits += other.edits;
    }
}

/// The confirmation for `ending` while `losses` are at work.
pub fn dialog(ending: Ending, losses: Losses) -> DialogText {
    let (title, doing, ok, id) = match ending {
        Ending::Window => (
            "Close the window?",
            "Closing the window",
            "Close Window",
            "close-window-description",
        ),
        Ending::App => ("Quit Flint?", "Quitting", "Quit", "quit-description"),
    };
    let Losses {
        live,
        forwards,
        edits,
    } = losses;
    let mut ends = Vec::new();
    let mut note = "";
    if !live.is_empty() {
        let (sessions, kept) = match live.total() {
            1 => ("1 session".to_owned(), " Its output is not kept."),
            n => (format!("{n} sessions"), " Their output is not kept."),
        };
        ends.push(format!(
            "ends {sessions} still running in the dock: {}",
            live.described()
        ));
        note = kept;
    }
    if forwards > 0 {
        ends.push(match forwards {
            1 => "stops 1 running port forward".to_owned(),
            n => format!("stops {n} running port forwards"),
        });
    }
    let mut text = format!("{doing} {}.{note}", ends.join(", and "));
    if edits > 0 {
        text.push_str(&match edits {
            1 => " It also discards 1 unapplied YAML edit, which cannot be undone.".to_owned(),
            n => format!(" It also discards {n} unapplied YAML edits, which cannot be undone."),
        });
    }
    DialogText {
        title: title.into(),
        text,
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
