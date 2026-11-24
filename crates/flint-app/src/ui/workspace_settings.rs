//! The workspace's side of the Settings view ([`settings_view`]): opening and leaving it, its
//! actions and their guards, Save and Revert, its keyboard cycle, its place in the main area and
//! the sidebar, and the Save / Discard / Cancel question when the window closes or the app quits
//! with a draft.
//!
//! Save hands the edited fields to the core ([`flint_core::CoreHandle::write_settings`]), which
//! writes each through the same setter the View menu, the Alerts menu and the header use, and
//! answers once they are written. Nothing about a setting written here is loosened. The edits
//! stay in the draft until that answer: a failed or unconfirmed Save keeps them.

// module-tests: settings_view_tests.rs

use std::rc::Rc;

use gpui_kit::component::{Theme, WindowExt as _};
use gpui_kit::*;

use super::super::confirm::{self, DialogText};
use super::super::registry::{Bind, Guards, OnView};
use super::super::settings_draft::Change;
use super::super::settings_view::{
    self, ActivateSettingsControl, CloseSettings, EditAppearance, EditDesktopNotifications,
    EditWatchMode, MainKey, NextSettingsControl, OpenSettings, PreviousSettingsControl,
    RevertSettings, SaveSettings, Saving, ShowSettingsSection, Stop, Then,
};
#[cfg(any(test, feature = "automation"))]
use super::super::view_state::SettingsState;
use super::Workspace;

/// How long a Save waits for the core's answer before it says it could not be confirmed.
const SAVE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What the footer says when the core did not answer a Save in time.
const UNCONFIRMED: &str =
    "The save could not be confirmed. Your edits are kept; save again to retry.";

impl Workspace {
    pub(super) fn settings_guards(guards: Guards<Self>) -> Guards<Self> {
        fn open(ws: &Workspace, _: &App) -> bool {
            ws.settings_page.open
        }
        // Not while a Save is being written: an edit then would be made over the old file.
        fn editable(ws: &Workspace, _: &App) -> bool {
            ws.settings_page.open && ws.settings_page.saving.is_none()
        }
        let guards = guards
            .when::<CloseSettings>(open)
            .when::<ShowSettingsSection>(open)
            .when::<NextSettingsControl>(open)
            .when::<PreviousSettingsControl>(open)
            .when::<ActivateSettingsControl>(open)
            .when::<EditAppearance>(editable)
            .when::<EditDesktopNotifications>(editable)
            .when::<EditWatchMode>(editable)
            .when::<SaveSettings>(|ws, cx| ws.can_save_settings(cx))
            .when::<RevertSettings>(|ws, _| {
                settings_view::can_revert(&ws.settings_page) && ws.settings_page.saving.is_none()
            });
        Self::logs_paths_guards(guards)
    }

    pub(in crate::ui) fn bind_settings_actions<B: Bind<Self>>(bind: B) -> B {
        let bind = Self::bind_logs_paths_actions(bind);
        bind.bind::<OpenSettings>(|ws, _, window, cx| ws.open_settings(window, cx))
            .bind::<CloseSettings>(|ws, _, window, cx| ws.close_settings(window, cx))
            .bind::<ShowSettingsSection>(|ws, action, _, cx| {
                ws.show_settings_section(action.section, cx)
            })
            .bind::<EditAppearance>(|ws, action, _, cx| {
                ws.edit_settings(Change::Appearance(action.appearance), cx)
            })
            .bind::<EditDesktopNotifications>(|ws, action, _, cx| {
                ws.edit_settings(Change::DesktopNotifications(action.enabled), cx)
            })
            .bind::<EditWatchMode>(|ws, action, _, cx| {
                ws.edit_settings(Change::WatchMode(action.mode), cx)
            })
            .bind::<SaveSettings>(|ws, _, window, cx| ws.save_settings(None, window, cx))
            .bind::<RevertSettings>(|ws, _, _, cx| ws.revert_settings(cx))
            .bind::<NextSettingsControl>(|ws, _, window, cx| {
                ws.move_settings_cursor(true, window, cx)
            })
            .bind::<PreviousSettingsControl>(|ws, _, window, cx| {
                ws.move_settings_cursor(false, window, cx)
            })
            .bind::<ActivateSettingsControl>(|ws, _, window, cx| {
                ws.activate_settings_control(window, cx)
            })
    }

    pub(super) fn register_settings_actions<E: InteractiveElement>(
        &self,
        element: E,
        cx: &Context<Self>,
    ) -> E {
        Self::bind_settings_actions(OnView::new(element, cx)).into_element()
    }

    /// Shows Settings in the main area, with focus in it.
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_page.open = true;
        self.settings_page.opened_over = Some(self.main_key());
        self.settings_page.cursor = None;
        self.ensure_settings_inputs(window, cx);
        window.focus(&self.settings_page.focus, cx);
        cx.notify();
    }

    /// Leaves Settings for what the main area showed before. The draft stays.
    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_page.open {
            return;
        }
        self.leave_settings_for_view();
        window.focus(&self.focus, cx);
        self.recover_focus(window, cx);
        cx.notify();
    }

    /// What the main area shows under Settings, as far as Settings gives way to it: another
    /// cluster or a set opened meanwhile. Views that open by themselves (a list restored, the
    /// overview once discovery ends) do not count; the navigation and Open Object leave
    /// Settings on their own.
    fn main_key(&self) -> MainKey {
        MainKey {
            cluster: self.cluster.clone(),
            set: self.sets.running.as_ref().map(|r| r.list.entity_id()),
        }
    }

    /// Leaves Settings when another cluster or a set was opened meanwhile. Called as the
    /// workspace draws.
    pub(super) fn settings_follow_main(&mut self) {
        if !self.settings_page.open {
            return;
        }
        let now = self.main_key();
        if self.settings_page.opened_over.as_ref() != Some(&now) {
            self.leave_settings_for_view();
        }
    }

    /// Leaves Settings for a view the navigation opens, even the one shown before.
    pub(super) fn leave_settings_for_view(&mut self) {
        self.settings_page.open = false;
        self.settings_page.opened_over = None;
        self.settings_page.cursor = None;
    }

    fn settings_invalid(&self, cx: &App) -> bool {
        self.model.read(cx).settings_error.is_some()
    }

    pub(super) fn can_save_settings(&self, cx: &App) -> bool {
        self.settings_page.saving.is_none()
            && settings_view::can_save(&self.settings_page, self.settings_invalid(cx))
    }

    fn show_settings_section(&mut self, section: settings_view::Section, cx: &mut Context<Self>) {
        if self.settings_page.open {
            self.settings_page.section = section;
            cx.notify();
        }
    }

    pub(super) fn edit_settings(&mut self, change: Change, cx: &mut Context<Self>) {
        if !self.settings_page.open || self.settings_page.saving.is_some() {
            return;
        }
        let file = self.model.read(cx).settings.clone();
        self.settings_page.draft.edit(change, &file);
        self.settings_page.save_note = None;
        cx.notify();
    }

    fn revert_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings_page.saving.is_some() {
            return;
        }
        self.settings_page.draft.revert();
        self.settings_page.save_note = None;
        // The fields show the file again, as the workspace next draws.
        self.settings_page.logs_paths.errors.clear();
        self.settings_page.logs_paths.kubeconfig_note = None;
        self.settings_page.logs_paths.resync = true;
        cx.notify();
    }

    /// Saves the draft. A draft that turns local cluster protection off asks first
    /// ([`Self::confirm_unprotect`]), whichever way Save was asked for: the button, the banner's
    /// Save anyway, the keyboard, the palette, or the quit and close question.
    fn save_settings(&mut self, then: Option<Then>, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_save_settings(cx) {
            return;
        }
        if self.settings_page.draft.loosens_protection() {
            self.confirm_unprotect(then, window, cx);
            return;
        }
        self.write_settings_draft(then, window, cx);
    }

    /// Hands the edited fields, and only those, to the core. They stay in the draft until it
    /// answers: written, they leave it and `then` runs; refused, or unanswered within
    /// [`SAVE_WAIT`], they stay, the footer says why, and `then` does not run. Nothing happens
    /// without edits, while `settings.toml` is invalid, or while an earlier Save is pending.
    /// Only [`Self::save_settings`] and a confirmed [`Self::confirm_unprotect`] call it.
    pub(super) fn write_settings_draft(
        &mut self,
        then: Option<Then>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_save_settings(cx) {
            return;
        }
        let writes = self.settings_page.draft.edits();
        let reply = self
            .model
            .read(cx)
            .core()
            .write_settings(writes.iter().map(|c| c.clone().into_write()).collect());
        let written = writes.clone();
        let answer = cx.spawn_in(window, async move |this, cx| {
            let result = reply.await;
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.finish_save(&written, result, window, cx)
            });
        });
        // A Save that changes local cluster protection is never given up on: the write could
        // still land after the view had let the user take it back, leaving protection off
        // against their last choice. Edits stay locked until the core answers.
        let changes_protection = writes
            .iter()
            .any(|c| matches!(c, Change::ProtectLocalClusters(_)));
        let give_up = cx.spawn(async move |this, cx| {
            if changes_protection {
                return;
            }
            cx.background_executor().timer(SAVE_WAIT).await;
            let _ = this.update(cx, |ws, cx| {
                if ws.settings_page.saving.take().is_some() {
                    ws.settings_page.save_note = Some(UNCONFIRMED.to_owned());
                    cx.notify();
                }
            });
        });
        self.settings_page.save_note = None;
        self.settings_page.saving = Some(Saving {
            after: then.into_iter().collect(),
            _answer: answer,
            _give_up: give_up,
        });
        cx.notify();
    }

    /// The core's answer to a Save of `writes`.
    fn finish_save(
        &mut self,
        writes: &[Change],
        result: Result<(), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saving) = self.settings_page.saving.take() else {
            return;
        };
        match result {
            Ok(()) => {
                self.settings_page.draft.saved(writes);
                for then in saving.after {
                    then(window, cx);
                }
            }
            Err(why) => {
                self.settings_page.save_note = Some(format!("Not saved: {why}"));
            }
        }
        cx.notify();
    }

    /// Moves the keyboard cursor to the next (or previous) control, round the view.
    fn move_settings_cursor(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_page.open {
            return;
        }
        let banner = self.settings_banner_shown(cx);
        let shown = self
            .settings_page
            .draft
            .shown(&self.model.read(cx).settings);
        let stops = settings_view::stops(self.settings_page.section, banner, &shown);
        let at = self
            .settings_page
            .cursor
            .and_then(|c| stops.iter().position(|s| *s == c));
        let next = match (at, forward) {
            (None, true) => 0,
            (None, false) => stops.len() - 1,
            (Some(i), true) => (i + 1) % stops.len(),
            (Some(i), false) => (i + stops.len() - 1) % stops.len(),
        };
        self.settings_page.cursor = Some(stops[next]);
        self.focus_settings_stop(window, cx);
        cx.notify();
    }

    /// Enter or Space: does what clicking the control under the cursor does.
    fn activate_settings_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(stop) = self.settings_page.cursor else {
            return;
        };
        match stop {
            Stop::Done => self.close_settings(window, cx),
            Stop::Reload | Stop::Revert => self.revert_settings(cx),
            Stop::SaveAnyway | Stop::Save => self.save_settings(None, window, cx),
            Stop::Section(section) => self.show_settings_section(section, cx),
            Stop::Appearance(appearance) => self.edit_settings(Change::Appearance(appearance), cx),
            Stop::DesktopNotifications => {
                let file = self.model.read(cx).settings.clone();
                let on = self.settings_page.draft.shown(&file).desktop_notifications;
                self.edit_settings(Change::DesktopNotifications(!on), cx)
            }
            Stop::WatchMode(mode) => self.edit_settings(Change::WatchMode(mode), cx),
            Stop::Text(_)
            | Stop::RemoveKubeconfig(_)
            | Stop::AddKubeconfig
            | Stop::ProtectLocalClusters => self.activate_logs_paths_stop(stop, window, cx),
        }
    }

    /// Whether the "modified outside Settings" banner shows.
    fn settings_banner_shown(&self, cx: &App) -> bool {
        settings_view::banner_shown(&self.settings_page, &self.model.read(cx).settings)
    }

    /// The Settings view in the main area, while it is open.
    pub(super) fn render_settings_page(
        &self,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.settings_page.open {
            return None;
        }
        let model = self.model.read(cx);
        Some(settings_view::render(
            &self.settings_page,
            &model.settings,
            model.settings_error.is_some(),
            theme,
        ))
    }

    /// The sidebar's Settings entry.
    pub(super) fn render_settings_entry(&self, theme: &Theme) -> impl IntoElement {
        settings_view::render_entry(&self.settings_page, theme)
    }

    #[cfg(any(test, feature = "automation"))]
    pub(super) fn settings_view_state(&self, cx: &App) -> SettingsState {
        let model = self.model.read(cx);
        self.settings_page
            .state(&model.settings, model.settings_error.is_some())
    }

    /// Whether Settings holds edits not yet written, a Save under way included.
    pub(in crate::ui) fn has_settings_draft(&self) -> bool {
        self.settings_page.draft.is_dirty()
    }

    /// Asks Save / Discard / Cancel about unsaved settings, if there are any; Save (once
    /// written) and Discard run `then`. While a Save is being written, `then` waits for it
    /// instead. Whether the caller must stop.
    pub(in crate::ui) fn asks_about_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        ending: &'static str,
        then: impl Fn(&mut Window, &mut App) + 'static,
    ) -> bool {
        self.ask_about_settings(window, cx, ending, Rc::new(then))
    }

    /// [`Self::asks_about_settings`], with `then` shared, so the question can ask again.
    fn ask_about_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        ending: &'static str,
        then: Then,
    ) -> bool {
        if !self.has_settings_draft() {
            return false;
        }
        // A Save already on its way: close or quit once it is written, never before.
        if let Some(saving) = &mut self.settings_page.saving {
            saving.after.push(then);
            return true;
        }
        if window.has_active_dialog(cx) {
            return true;
        }
        let invalid = self.settings_invalid(cx);
        let fields_invalid = !self.settings_page.logs_paths.errors.is_empty();
        let workspace = cx.weak_entity();
        if invalid || fields_invalid {
            let why = if invalid {
                "settings.toml is invalid"
            } else {
                "a field in Settings has a value that cannot be saved"
            };
            let text = DialogText {
                title: "Discard unsaved settings?".into(),
                text: format!(
                    "{why}, so your edits in Settings cannot be saved. {ending} discards them."
                ),
                ok: "Discard".into(),
                cancel: "Cancel".into(),
                id: "settings-draft-description",
                // Discarding loses the edits: only the button does it, Enter cancels.
                default_cancel: true,
            };
            confirm::open(window, cx, text, move |window, cx| {
                let then = then.clone();
                let _ = workspace.update(cx, |ws, cx| ws.revert_settings(cx));
                then(window, cx);
            });
            return true;
        }
        let text = DialogText {
            title: "Save your settings?".into(),
            text: format!(
                "Settings has unsaved edits. Save writes them to settings.toml; Discard drops \
                 them. {ending} goes on either way."
            ),
            ok: "Save".into(),
            cancel: "Cancel".into(),
            id: "settings-draft-description",
            default_cancel: false,
        };
        let (save_ws, save_then) = (workspace.clone(), then.clone());
        confirm::open_with_alternative(
            window,
            cx,
            text,
            "Discard".into(),
            move |window, cx| {
                let then = save_then.clone();
                let _ = save_ws.update(cx, |ws, cx| {
                    if ws.can_save_settings(cx) {
                        ws.save_settings(Some(then), window, cx);
                    } else {
                        // The file turned invalid since the question opened: ask again, which
                        // says why.
                        ws.ask_about_settings(window, cx, ending, then);
                    }
                });
            },
            move |window, cx| {
                let _ = workspace.update(cx, |ws, cx| ws.revert_settings(cx));
                then(window, cx);
            },
        );
        true
    }
}
