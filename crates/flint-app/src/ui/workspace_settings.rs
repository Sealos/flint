//! The workspace's side of the Settings view ([`settings_view`]): opening and leaving it, its
//! actions and their guards, Save and Revert, its place in the main area and the sidebar, and
//! the Save / Discard / Cancel question when the window closes or the app quits with a draft.
//!
//! Save writes each edited field through the core's own command for it, the same one the View
//! menu, the Alerts menu and the header use, so a setting written here goes through the same
//! checks as from anywhere else and nothing about it is loosened.

// module-tests: settings_view_tests.rs

use gpui_kit::component::{Theme, WindowExt as _};
use gpui_kit::*;

use super::super::clusters::ClustersModel;
use super::super::confirm::{self, DialogText};
use super::super::registry::{Bind, Guards, OnView};
use super::super::settings_draft::Change;
use super::super::settings_view::{
    self, CloseSettings, EditAppearance, EditDesktopNotifications, EditWatchMode, MainKey,
    OpenSettings, RevertSettings, SaveSettings, ShowSettingsSection,
};
#[cfg(any(test, feature = "automation"))]
use super::super::view_state::SettingsState;
use super::Workspace;

/// What closing or quitting does once the settings question is answered.
type Then = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// How long a Save waits for the core to report it written before the controls take edits
/// again.
const SAVE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Sends `change` to the core, through the command each other control uses for that setting.
fn write(change: Change, model: &mut ClustersModel) {
    match change {
        Change::Appearance(appearance) => model.set_appearance(appearance),
        Change::DesktopNotifications(enabled) => model.set_desktop_notifications(enabled),
        Change::WatchMode(mode) => model.set_watch_mode(None, Some(mode)),
    }
}

impl Workspace {
    pub(super) fn settings_guards(guards: Guards<Self>) -> Guards<Self> {
        // Not while a Save is being written: an edit then would be made over the old file.
        fn open(ws: &Workspace, _: &App) -> bool {
            ws.settings_page.open && ws.settings_page.saving.is_none()
        }
        guards
            .when::<CloseSettings>(open)
            .when::<ShowSettingsSection>(open)
            .when::<EditAppearance>(open)
            .when::<EditDesktopNotifications>(open)
            .when::<EditWatchMode>(open)
            .when::<SaveSettings>(|ws, cx| ws.can_save_settings(cx))
            .when::<RevertSettings>(|ws, _| ws.settings_page.draft.is_dirty())
    }

    pub(in crate::ui) fn bind_settings_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<OpenSettings>(|ws, _, window, cx| ws.open_settings(window, cx))
            .bind::<CloseSettings>(|ws, _, window, cx| ws.close_settings(window, cx))
            .bind::<ShowSettingsSection>(|ws, action, _, cx| {
                if ws.settings_page.open {
                    ws.settings_page.section = action.section;
                    cx.notify();
                }
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
            .bind::<SaveSettings>(|ws, _, window, cx| ws.save_settings(window, cx, |_, _, _| {}))
            .bind::<RevertSettings>(|ws, _, _, cx| ws.revert_settings(cx))
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
        window.focus(&self.settings_page.focus, cx);
        cx.notify();
    }

    /// Leaves Settings for what the main area showed before. The draft stays.
    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_page.open {
            return;
        }
        self.settings_page.open = false;
        self.settings_page.opened_over = None;
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

    /// Leaves Settings when something else was opened in the main area meanwhile. Called as the
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
    }

    fn settings_invalid(&self, cx: &App) -> bool {
        self.model.read(cx).settings_error.is_some()
    }

    fn can_save_settings(&self, cx: &App) -> bool {
        self.settings_page.saving.is_none()
            && settings_view::can_save(&self.settings_page.draft, self.settings_invalid(cx))
    }

    fn edit_settings(&mut self, change: Change, cx: &mut Context<Self>) {
        if !self.settings_page.open || self.settings_page.saving.is_some() {
            return;
        }
        let file = self.model.read(cx).settings.clone();
        self.settings_page.draft.edit(change, &file);
        cx.notify();
    }

    fn revert_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_page.draft.revert();
        cx.notify();
    }

    /// Writes the edited fields, and only those, then runs `then` once the core reports them
    /// written. If the core reports an error first, or nothing within [`SAVE_WAIT`], `then` does
    /// not run and the error notice says what failed. Nothing happens without edits, while
    /// `settings.toml` is invalid, or while an earlier Save is still being written.
    fn save_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if !self.can_save_settings(cx) {
            return;
        }
        let writes = self.settings_page.draft.take();
        // Only an error reported after these writes were sent counts as theirs.
        let errors_before = self.model.read(cx).errors_seen;
        self.model.update(cx, |model, _| {
            for change in &writes {
                write(*change, model);
            }
        });
        let landed = {
            let file = &self.model.read(cx).settings;
            writes.iter().all(|change| change.holds_in(file))
        };
        // The file has these values already: the core writes nothing new and reports nothing.
        if landed {
            cx.notify();
            then(self, window, cx);
            return;
        }
        let mut then = Some(then);
        let watch = cx.observe_in(&self.model, window, move |ws, model, window, cx| {
            let model = model.read(cx);
            if writes.iter().all(|change| change.holds_in(&model.settings)) {
                ws.end_saving(cx);
                if let Some(then) = then.take() {
                    then(ws, window, cx);
                }
            } else if model.errors_seen > errors_before {
                ws.end_saving(cx);
            }
        });
        let give_up = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_WAIT).await;
            let _ = this.update(cx, |ws, cx| ws.end_saving(cx));
        });
        self.settings_page.saving = Some((watch, give_up));
        cx.notify();
    }

    /// Stops waiting for a Save: the controls and Save take edits again.
    fn end_saving(&mut self, cx: &mut Context<Self>) {
        if self.settings_page.saving.take().is_some() {
            cx.notify();
        }
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

    /// Whether Settings holds unsaved edits.
    pub(in crate::ui) fn has_settings_draft(&self) -> bool {
        self.settings_page.draft.is_dirty()
    }

    /// Asks Save / Discard / Cancel about unsaved settings, if there are any; Save (once
    /// written) and Discard run `then`. Whether the caller must stop.
    pub(in crate::ui) fn asks_about_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        ending: &'static str,
        then: impl Fn(&mut Window, &mut App) + 'static,
    ) -> bool {
        self.ask_about_settings(window, cx, ending, std::rc::Rc::new(then))
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
        if window.has_active_dialog(cx) {
            return true;
        }
        let invalid = self.settings_invalid(cx);
        let workspace = cx.weak_entity();
        if invalid {
            let text = DialogText {
                title: "Discard unsaved settings?".into(),
                text: format!(
                    "settings.toml is invalid, so your edits in Settings cannot be saved. \
                     {ending} discards them."
                ),
                ok: "Discard".into(),
                cancel: "Cancel".into(),
                id: "settings-draft-description",
                default_cancel: false,
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
                        ws.save_settings(window, cx, move |_, window, cx| then(window, cx));
                    } else {
                        // The file turned invalid (or a Save is under way) since the question
                        // opened: ask again, which says why.
                        ws.ask_about_settings(window, cx, ending, then.clone());
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
