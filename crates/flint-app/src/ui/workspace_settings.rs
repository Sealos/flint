//! The workspace's side of the Settings view ([`settings_view`]): the modal it is drawn in,
//! opening and closing it (Discard / Keep editing with unsaved edits), its actions and their
//! guards, Save and Revert, its keyboard cycle, the sidebar's entry, and the Save / Discard /
//! Cancel question when the window closes or the app quits with a draft.
//!
//! The modal is a gpui-kit dialog, drawn in the window's dialog layer rather than inside the
//! workspace, so [`SettingsModal`] draws the workspace's page there and forwards the Settings
//! actions to the workspace ([`Forward`]). Confirmations asked from inside it (turning
//! protection off, applying a proxy, discarding) open above it, and focus goes back to where it
//! was once it closes.
//!
//! Save hands the edited fields to the core ([`flint_core::CoreHandle::write_settings`]), which
//! writes them all in one save, each as the setter the View menu, the Alerts menu and the header
//! use writes it, and answers once they are written: all of them, or none. Nothing about a setting written here is loosened. The edits
//! stay in the draft until that answer: a failed or unconfirmed Save keeps them.

// module-tests: settings_view_tests.rs

use std::rc::Rc;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{ActiveTheme as _, Theme, WindowExt as _};
use gpui_kit::*;

use super::super::confirm::{self, DialogText};
use super::super::registry::{Bind, Forward, Guards, OnView};
use super::super::settings_draft::{Change, SettingsDraft};
use super::super::settings_view::{
    self, ActivateSettingsControl, CloseSettings, EditAppearance, EditDesktopNotifications,
    EditWatchMode, NextSettingsControl, OpenSettings, PreviousSettingsControl, RevertSettings,
    SaveSettings, Saving, ShowSettingsSection, Stop, Then,
};
#[cfg(any(test, feature = "automation"))]
use super::super::view_state::SettingsState;
use super::Workspace;
use flint_core::settings::{PROTECTION_CONFIRMATION_STALE, SettingsRevision};

/// How long a Save waits for the core's answer before it says it could not be confirmed.
const SAVE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What the footer says when the core did not answer a Save in time.
const UNCONFIRMED: &str =
    "The save could not be confirmed. Your edits are kept; save again to retry.";

/// What the footer says when a Save asked for by quit or close went through but edits were made
/// while it was being written: the quit or close waits for them.
const EDITED_WHILE_SAVING: &str =
    "Saved. Edits made while saving are not saved yet; save or revert them, then try again.";

/// The widest the Settings modal gets; a narrower window narrows it.
const MODAL_WIDTH: f32 = 960.;
/// The tallest the Settings modal gets.
const MODAL_HEIGHT: f32 = 680.;
/// What the modal leaves of the window above and below it, at least.
const MODAL_MARGIN: f32 = 48.;

/// What closing Settings with unsaved edits asks.
pub(in crate::ui) const DISCARD_TITLE: &str = "Close Settings without saving?";

/// The Settings page drawn in its modal, over the workspace that holds it. Drawn again whenever
/// the workspace changes, since the dialog layer is not part of the workspace's frame.
struct SettingsModal {
    workspace: WeakEntity<Workspace>,
    _redraw: Subscription,
}

impl Render for SettingsModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(workspace) = self.workspace.upgrade() else {
            return div().into_any_element();
        };
        let height = (window.viewport_size().height - px(2. * MODAL_MARGIN))
            .min(px(MODAL_HEIGHT))
            .max(px(0.));
        let page = workspace.read(cx).render_settings_page(cx.theme(), cx);
        let modal = div()
            .id("settings-modal")
            .w_full()
            .h(height)
            .flex()
            .children(page)
            .test_support();
        // Keys pressed in the modal go up the dialog layer, not the workspace: handled here.
        Workspace::bind_settings_actions(Forward::new(modal, Some(&workspace)))
            .into_element()
            .into_any_element()
    }
}

impl Workspace {
    pub(super) fn settings_guards(guards: Guards<Self>) -> Guards<Self> {
        fn open(ws: &Workspace, _: &App) -> bool {
            ws.settings_page.open
        }
        // Not while a Save is being written: an edit then would be made over the old file.
        fn editable(ws: &Workspace, _: &App) -> bool {
            ws.settings_page.open && ws.settings_page.saving.is_none()
        }
        // Show Settings Section opens Settings when it is closed.
        Self::network_guards(Self::logs_paths_guards(Self::threshold_guards(guards)))
            .when::<CloseSettings>(open)
            .when::<NextSettingsControl>(open)
            .when::<PreviousSettingsControl>(open)
            .when::<ActivateSettingsControl>(open)
            .when::<EditAppearance>(editable)
            .when::<EditDesktopNotifications>(editable)
            .when::<EditWatchMode>(editable)
            // A threshold still being typed is entered by Save, which then checks the draft,
            // and dropped by Revert.
            // With a value rejected, Save shows its field instead of writing.
            .when::<SaveSettings>(|ws, cx| {
                ws.can_save_settings(cx)
                    || ws.settings_field_error_to_show()
                    || (ws.settings_page.thresholds.entry.is_some()
                        && ws.settings_page.saving.is_none()
                        && !ws.settings_invalid(cx))
            })
            // Revert also closes a field left open, as its button shows.
            .when::<RevertSettings>(|ws, _| {
                (ws.settings_page.draft.is_dirty()
                    || ws.settings_page.thresholds.entry.is_some()
                    || ws.settings_page.network.is_typed())
                    && ws.settings_page.saving.is_none()
            })
    }

    pub(in crate::ui) fn bind_settings_actions<B: Bind<Self>>(bind: B) -> B {
        Self::bind_network_actions(Self::bind_logs_paths_actions(Self::bind_threshold_actions(
            bind,
        )))
        .bind::<OpenSettings>(|ws, _, window, cx| ws.open_settings(window, cx))
        .bind::<CloseSettings>(|ws, _, window, cx| ws.close_settings(window, cx))
        .bind::<ShowSettingsSection>(|ws, action, window, cx| {
            ws.show_settings_section(action.section, window, cx)
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
        .bind::<RevertSettings>(|ws, _, window, cx| {
            ws.finish_threshold_entry(false, window, cx);
            ws.revert_settings(cx)
        })
        .bind::<NextSettingsControl>(|ws, _, window, cx| ws.move_settings_cursor(true, window, cx))
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

    /// Opens the Settings modal, with focus in it; with it open already, focus goes back into
    /// it. Not over a confirmation still waiting for an answer.
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_page.open {
            if !confirm::any_open(window, cx) {
                window.focus(&self.settings_page.focus, cx);
            }
            return;
        }
        if confirm::any_open(window, cx) {
            return;
        }
        self.settings_page.open = true;
        self.settings_page.cursor = None;
        self.ensure_settings_inputs(window, cx);
        self.ensure_network_inputs(window, cx);
        let workspace = cx.entity();
        let modal = cx.new(|cx| SettingsModal {
            workspace: workspace.downgrade(),
            _redraw: cx.observe(&workspace, |_, _, cx| cx.notify()),
        });
        let close = workspace.downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let close = close.clone();
            #[cfg(feature = "automation")]
            let dialog = dialog.entrance(gpui_kit::component::dialog::DialogEntrance::None);
            dialog
                .w(px(MODAL_WIDTH))
                .p_0()
                .close_button(false)
                .child(modal.clone())
                // Enter belongs to the control under the cursor, never to the modal.
                .on_ok(|_, _, _| false)
                // Esc in a text field, or a click outside: the workspace decides, and closes it.
                .on_cancel(move |_, window, cx| {
                    let _ = close.update(cx, |ws, cx| ws.close_settings(window, cx));
                    false
                })
        });
        // The dialog took focus as it opened: hand it on to the view.
        window.focus(&self.settings_page.focus, cx);
        cx.notify();
    }

    /// Closes Settings: at once without unsaved edits, otherwise after asking Discard / Keep
    /// editing. A Save under way closes it once written. Only while Settings is the newest
    /// dialog: a confirmation over it is answered first.
    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_page.open || confirm::any_open(window, cx) {
            return;
        }
        if let Some(saving) = &mut self.settings_page.saving {
            let workspace = cx.weak_entity();
            // Run inside the workspace's update (finish_save): closing waits for it to end.
            saving
                .after
                .push(Rc::new(move |window: &mut Window, cx: &mut App| {
                    let workspace = workspace.clone();
                    window.defer(cx, move |window, cx| {
                        let _ = workspace.update(cx, |ws, cx| ws.close_settings(window, cx));
                    });
                }));
            return;
        }
        if !self.settings_unsaved(cx) {
            self.dismiss_settings(window, cx);
            return;
        }
        let text = DialogText {
            title: DISCARD_TITLE.into(),
            text: "Settings has unsaved edits. Discard drops them and closes Settings.".into(),
            ok: "Discard".into(),
            cancel: "Keep editing".into(),
            id: "settings-discard-description",
            // Dropping the edits takes the button: Enter keeps editing.
            default_cancel: true,
        };
        let workspace = cx.weak_entity();
        confirm::open(window, cx, text, move |window, cx| {
            let workspace = workspace.clone();
            // Once this confirmation has closed, so that Settings is the newest dialog again.
            window.defer(cx, move |window, cx| {
                let _ = workspace.update(cx, |ws, cx| {
                    if ws.settings_page.open
                        && ws.settings_page.saving.is_none()
                        && !confirm::any_open(window, cx)
                    {
                        ws.dismiss_settings(window, cx);
                    }
                });
            });
        });
    }

    /// Whether closing Settings would lose something: the draft, a field still open, or proxy
    /// text not applied.
    fn settings_unsaved(&self, cx: &App) -> bool {
        self.has_settings_draft(cx)
            || self.settings_page.thresholds.entry.is_some()
            || self.settings_page.network.is_typed()
    }

    /// Drops whatever Settings holds and closes the modal, which gives focus back to where it
    /// was when Settings opened.
    fn dismiss_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.forget_settings(window, cx);
        window.close_dialog(cx);
    }

    /// Drops the draft and what the sections hold, and marks Settings closed.
    fn forget_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_threshold_entry(false, window, cx);
        self.revert_settings(cx);
        self.discard_network_fields();
        self.settings_page.open = false;
        self.settings_page.cursor = None;
        cx.notify();
    }

    /// Forgets the modal when its dialog was closed by something other than Settings itself.
    /// Called as the workspace draws.
    pub(super) fn settings_follow_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_page.open {
            return;
        }
        if !window.has_active_dialog(cx) {
            self.forget_settings(window, cx);
            return;
        }
        // Focus is trapped in the modal: a view behind it that took focus (the overview once
        // discovery ends, a list restored) gives it back, unless a confirmation holds it.
        let page = &self.settings_page;
        // A field just opened is not drawn yet, so the view does not contain it until it is.
        let in_settings = page.focus.contains_focused(window, cx)
            || page
                .thresholds
                .entry
                .as_ref()
                .is_some_and(|entry| entry.input.focus_handle(cx).is_focused(window))
            || page.logs_paths.focused(window, cx).is_some()
            || page.network.focused(window, cx).is_some();
        let focus = page.focus.clone();
        if !in_settings && !confirm::any_open(window, cx) {
            window.defer(cx, move |window, cx| {
                if !confirm::any_open(window, cx) {
                    window.focus(&focus, cx);
                }
            });
        }
    }

    fn settings_invalid(&self, cx: &App) -> bool {
        self.model.read(cx).settings_error.is_some()
    }

    pub(super) fn can_save_settings(&self, cx: &App) -> bool {
        self.settings_page.saving.is_none()
            && settings_view::can_save(&self.settings_page.draft, self.settings_invalid(cx))
    }

    pub(super) fn show_settings_section(
        &mut self,
        section: settings_view::Section,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings_page.open {
            self.open_settings(window, cx);
        }
        if self.settings_page.open {
            self.finish_threshold_entry(true, window, cx);
            self.settings_page.section = section;
            // The cursor stays on the section list, never on a control no longer drawn.
            if self.settings_page.cursor.is_some() {
                self.settings_page.cursor = Some(Stop::Section(section));
            }
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
        self.revert_thresholds_page();
        self.settings_page.save_note = None;
        // The text fields show the file again, as the workspace next draws.
        self.settings_page.logs_paths.kubeconfig_note = None;
        self.settings_page.logs_paths.resync = true;
        self.revert_network_fields();
        cx.notify();
    }

    /// Whether a value typed is rejected, which Save shows rather than writes.
    fn settings_field_error_to_show(&self) -> bool {
        self.settings_page.open
            && self.settings_page.saving.is_none()
            && self.settings_page.draft.has_rejected()
    }

    /// Saves the draft. A draft that turns local cluster protection off asks first
    /// ([`Self::confirm_unprotect`]), whichever way Save was asked for: the button, the banner's
    /// Save anyway, the keyboard, the palette, or the quit and close question. While a value
    /// typed is rejected, nothing is written: Save shows its field, scrolled into view.
    fn save_settings(&mut self, then: Option<Then>, window: &mut Window, cx: &mut Context<Self>) {
        // A threshold still being typed is part of what is saved (or, rejected, blocks it).
        self.finish_threshold_entry(true, window, cx);
        if self.settings_field_error_to_show() {
            self.show_settings_field_error(window, cx);
            return;
        }
        if !self.can_save_settings(cx) {
            return;
        }
        if self.settings_page.draft.loosens_protection() {
            self.confirm_unprotect(then, window, cx);
            return;
        }
        self.write_settings_draft(then, None, window, cx);
    }

    /// Hands the edited fields, and only those, to the core, in one save. They stay in the draft
    /// until it answers: written, they leave it and `then` runs; refused, or unanswered within
    /// [`SAVE_WAIT`], they stay, the footer says why, and `then` does not run. Nothing happens
    /// without edits, while `settings.toml` is invalid, or while an earlier Save is pending.
    /// Only [`Self::save_settings`] and a confirmed [`Self::confirm_unprotect`] call it; the
    /// latter passes `confirmed`, the settings it was confirmed over, without which the core
    /// refuses to turn protection off.
    pub(super) fn write_settings_draft(
        &mut self,
        then: Option<Then>,
        confirmed: Option<SettingsRevision>,
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
            .write_settings(SettingsDraft::confirmed(&writes, confirmed.as_ref()));
        let written = writes.clone();
        let answer = cx.spawn_in(window, async move |this, cx| {
            let result = reply.await;
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.finish_save(&written, result, window, cx)
            });
        });
        // A Save that turns local cluster protection off is never given up on: the write could
        // still land after the view had let the user take it back, leaving protection off
        // against their last choice. Edits stay locked until the core answers.
        let unprotects = self.settings_page.draft.loosens_protection();
        let give_up = cx.spawn(async move |this, cx| {
            if unprotects {
                return;
            }
            cx.background_executor().timer(SAVE_WAIT).await;
            let _ = this.update(cx, |ws, cx| {
                if ws.settings_page.saving.take().is_some() {
                    ws.take_typed_while_saving(None, cx);
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

    /// The core's answer to a Save of `writes`, all written or none. Text typed into a field
    /// meanwhile joins the draft first, so it is neither lost nor taken for saved. `then` (quit,
    /// close) runs only once everything is saved and nothing new waits.
    fn finish_save(
        &mut self,
        writes: &[Change],
        result: Result<std::sync::Arc<flint_core::Settings>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saving) = self.settings_page.saving.take() else {
            return;
        };
        self.take_typed_while_saving(result.as_deref().ok(), cx);
        match result {
            Ok(_) => {
                self.settings_page.draft.saved(writes);
                self.saved_thresholds_page();
                if self.has_settings_draft(cx) {
                    if !saving.after.is_empty() {
                        self.settings_page.save_note = Some(EDITED_WHILE_SAVING.to_owned());
                    }
                } else {
                    for then in saving.after {
                        then(window, cx);
                    }
                }
            }
            Err(why) => {
                self.settings_page.save_note = Some(format!("Not saved: {why}"));
                // Turning protection off was confirmed over a file that changed since: ask
                // again, over the file as it is now.
                if why.contains(PROTECTION_CONFIRMATION_STALE)
                    && self.settings_page.draft.loosens_protection()
                {
                    let after = saving.after;
                    let then: Option<Then> = (!after.is_empty()).then(|| {
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            for then in &after {
                                then(window, cx);
                            }
                        }) as Then
                    });
                    self.confirm_unprotect(then, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Moves the keyboard cursor to the next (or previous) control, round the view.
    fn move_settings_cursor(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_page.open {
            return;
        }
        self.follow_threshold_rows(cx);
        let banner = self.settings_banner_shown(cx);
        let layout = self.overrides_layout(cx);
        let kubeconfigs = self.settings_kubeconfigs(cx);
        let stops = settings_view::stops(self.settings_page.section, banner, layout, kubeconfigs);
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
        self.track_threshold_cursor(cx);
        self.scroll_to_settings_cursor(cx);
        self.focus_settings_stop(window, cx);
        cx.notify();
    }

    /// Enter or Space: does what clicking the control under the cursor does.
    fn activate_settings_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.follow_threshold_rows(cx);
        let Some(stop) = self.settings_page.cursor else {
            return;
        };
        // Only a control drawn now: never one of a section no longer shown.
        let banner = self.settings_banner_shown(cx);
        let layout = self.overrides_layout(cx);
        let kubeconfigs = self.settings_kubeconfigs(cx);
        if !settings_view::stops(self.settings_page.section, banner, layout, kubeconfigs)
            .contains(&stop)
        {
            return;
        }
        match stop {
            Stop::Close => self.close_settings(window, cx),
            Stop::Reload | Stop::Revert => {
                self.finish_threshold_entry(false, window, cx);
                self.revert_settings(cx)
            }
            Stop::SaveAnyway | Stop::Save => self.save_settings(None, window, cx),
            Stop::Section(section) => self.show_settings_section(section, window, cx),
            Stop::Appearance(appearance) => self.edit_settings(Change::Appearance(appearance), cx),
            Stop::DesktopNotifications => {
                let file = self.model.read(cx).settings.clone();
                let on = self.settings_page.draft.shown(&file).desktop_notifications;
                self.edit_settings(Change::DesktopNotifications(!on), cx)
            }
            Stop::WatchMode(mode) => self.edit_settings(Change::WatchMode(mode), cx),
            Stop::Threshold(stop) => self.activate_threshold_stop(stop, window, cx),
            Stop::Network(stop) => self.activate_network_stop(stop, window, cx),
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

    /// The Settings view, drawn in its modal, while it is open.
    fn render_settings_page(&self, theme: &Theme, cx: &App) -> Option<AnyElement> {
        if !self.settings_page.open {
            return None;
        }
        let clusters = self.settings_clusters(cx);
        let model = self.model.read(cx);
        Some(settings_view::render(
            &self.settings_page,
            &model.settings,
            model.settings_error.is_some(),
            &clusters,
            theme,
        ))
    }

    /// The sidebar's Settings entry.
    pub(super) fn render_settings_entry(&self, theme: &Theme) -> impl IntoElement {
        settings_view::render_entry(&self.settings_page, theme)
    }

    #[cfg(any(test, feature = "automation"))]
    pub(super) fn settings_view_state(&self, cx: &App) -> SettingsState {
        let clusters = self.settings_clusters(cx);
        let model = self.model.read(cx);
        self.settings_page
            .state(&model.settings, model.settings_error.is_some(), &clusters)
    }

    /// Whether Settings holds edits not yet written, a Save under way included: the draft (its
    /// rejected values too), a threshold being typed, or text typed while a Save was written.
    pub(in crate::ui) fn has_settings_draft(&self, cx: &App) -> bool {
        self.settings_page.draft.is_dirty()
            || !self.settings_page.logs_paths.typed_while_saving.is_empty()
            || self
                .settings_page
                .thresholds
                .entry
                .as_ref()
                .is_some_and(|entry| entry.typed(cx).is_some())
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
        // A threshold still being typed counts as an edit.
        self.finish_threshold_entry(true, window, cx);
        if !self.has_settings_draft(cx) {
            return false;
        }
        // A Save already on its way: close or quit once it is written, never before.
        if let Some(saving) = &mut self.settings_page.saving {
            saving.after.push(then);
            return true;
        }
        if confirm::any_open(window, cx) {
            return true;
        }
        let invalid = self.settings_invalid(cx);
        let rejected = self.settings_page.draft.has_rejected();
        let workspace = cx.weak_entity();
        if invalid || rejected {
            let why = if invalid {
                "settings.toml is invalid"
            } else {
                "Some values in Settings are not accepted"
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
