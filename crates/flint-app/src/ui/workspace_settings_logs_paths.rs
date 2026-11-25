//! The workspace's side of the Logs, Paths and Write safety sections of Settings
//! ([`super::super::settings_logs_paths`]): their actions and guards, the text fields, the kubeconfig file
//! picker, and the confirmation that Save asks before it turns local cluster protection off.
//!
//! Turning protection off never happens silently. Whichever way Save is asked for, a draft that
//! turns it off opens a confirmation first ([`Workspace::confirm_unprotect`]); Enter and Escape
//! cancel it, only its button confirms. Cancel keeps the whole draft unsaved. The confirmation
//! holds for the draft and the file it was asked over: if either changed before it was
//! answered (an edit, or `settings.toml` changed on disk), confirming saves nothing and the
//! footer says to review and save again. Turning protection on is an ordinary edit. The core
//! still decides what each write may touch ([`flint_core::protection`]).

// module-tests: settings_logs_paths_tests.rs

use std::path::PathBuf;
use std::sync::Arc;

use flint_core::Settings;
use flint_core::settings::SettingsRevision;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;

use super::super::confirm::{self, DialogText};
use super::super::registry::{Bind, Guards};
use super::super::settings_draft::Change;
use super::super::settings_logs_paths::{
    AddKubeconfigPath, ChooseKubeconfigPaths, EditHelmPath, EditLogBufferLines, EditLogBufferMib,
    EditLogTotalMib, EditPathOverride, EditProtectLocalClusters, Inputs, RemoveKubeconfigPath,
    TextField, UNPROTECT_ON_DISK, UNPROTECT_STALE, UNPROTECT_TEXT, UNPROTECT_TITLE, content_row,
    with_kubeconfig,
};
use super::super::settings_view::{self, Section, Stop, Then};
use super::Workspace;

/// The edit that turns local cluster protection on or off. Turning it off is confirmed at Save,
/// which adds what it was confirmed over.
fn protect(on: bool) -> Change {
    Change::ProtectLocalClusters {
        on,
        confirmed: None,
    }
}

/// The user's home folder, to check paths that start with `~`.
fn home() -> Option<PathBuf> {
    std::env::home_dir()
}

impl Workspace {
    pub(super) fn logs_paths_guards(guards: Guards<Self>) -> Guards<Self> {
        // As the other edits: not while a Save is being written.
        fn editable(ws: &Workspace, _: &App) -> bool {
            ws.settings_page.open && ws.settings_page.saving.is_none()
        }
        guards
            .when::<EditLogBufferLines>(editable)
            .when::<EditLogBufferMib>(editable)
            .when::<EditLogTotalMib>(editable)
            .when::<EditPathOverride>(editable)
            .when::<EditHelmPath>(editable)
            .when::<AddKubeconfigPath>(editable)
            .when::<RemoveKubeconfigPath>(editable)
            .when::<ChooseKubeconfigPaths>(editable)
            .when::<EditProtectLocalClusters>(editable)
    }

    pub(super) fn bind_logs_paths_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<EditLogBufferLines>(|ws, action, window, cx| {
            ws.set_settings_text(
                TextField::LogBufferLines,
                action.lines.to_string(),
                window,
                cx,
            )
        })
        .bind::<EditLogBufferMib>(|ws, action, window, cx| {
            ws.set_settings_text(TextField::LogBufferMib, action.mib.to_string(), window, cx)
        })
        .bind::<EditLogTotalMib>(|ws, action, window, cx| {
            ws.set_settings_text(TextField::LogTotalMib, action.mib.to_string(), window, cx)
        })
        .bind::<EditPathOverride>(|ws, action, window, cx| {
            ws.set_settings_text(TextField::Path, action.path.clone(), window, cx)
        })
        .bind::<EditHelmPath>(|ws, action, window, cx| {
            ws.set_settings_text(TextField::Helm, action.path.clone(), window, cx)
        })
        .bind::<AddKubeconfigPath>(|ws, action, _, cx| {
            ws.add_kubeconfig_paths(vec![PathBuf::from(&action.path)], cx)
        })
        .bind::<RemoveKubeconfigPath>(|ws, action, _, cx| {
            ws.remove_kubeconfig_path(&PathBuf::from(&action.path), action.index, cx)
        })
        .bind::<ChooseKubeconfigPaths>(|ws, _, _, cx| ws.choose_kubeconfig_paths(cx))
        .bind::<EditProtectLocalClusters>(|ws, action, _, cx| {
            ws.edit_settings(protect(action.enabled), cx)
        })
    }

    /// Makes the text fields, the first time Settings opens in this window.
    pub(super) fn ensure_settings_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_page.logs_paths.inputs.is_some() {
            return;
        }
        let shown = self.settings_shown(cx);
        let mut fields = Vec::new();
        let mut subscriptions = Vec::new();
        for field in TextField::ALL {
            let text = field.text(&shown);
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(field.placeholder())
                    .default_value(text)
            });
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |ws: &mut Workspace, input: &Entity<InputState>, event, window, cx| {
                    match event {
                        InputEvent::Change => {
                            let text = input.read(cx).value().to_string();
                            ws.apply_settings_text(field, &text, cx);
                        }
                        // A click into the field puts the keyboard cursor on it, so Tab goes
                        // on from there.
                        InputEvent::Focus => {
                            if ws.settings_page.open {
                                ws.settings_page.cursor = Some(Stop::Text(field));
                                cx.notify();
                            }
                        }
                        // Enter leaves the field for the view, the cursor still on it.
                        InputEvent::PressEnter { .. } => {
                            window.focus(&ws.settings_page.focus, cx);
                        }
                        _ => {}
                    }
                },
            ));
            fields.push((field, input));
        }
        self.settings_page.logs_paths.inputs = Some(Inputs {
            fields,
            _subscriptions: subscriptions,
        });
    }

    /// What the view shows: the file with the draft on top.
    fn settings_shown(&self, cx: &App) -> Settings {
        self.settings_page
            .draft
            .shown(&self.model.read(cx).settings)
    }

    /// Puts what the view shows into every field that is not being typed in and holds no
    /// invalid text: after a Save, or a change to `settings.toml`. After Revert or Reload, into
    /// every field, the focused one too. Text typed while a Save was being written is read again
    /// once it is answered. Called as the workspace draws.
    pub(super) fn sync_settings_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_page.open || self.settings_page.logs_paths.inputs.is_none() {
            return;
        }
        let resync = std::mem::take(&mut self.settings_page.logs_paths.resync);
        let page = &self.settings_page;
        let Some(inputs) = &page.logs_paths.inputs else {
            return;
        };
        let shown = self.settings_shown(cx);
        let focused = if resync {
            None
        } else {
            page.logs_paths.focused(window, cx)
        };
        let stale: Vec<(Entity<InputState>, String)> = inputs
            .fields
            .iter()
            .filter(|(field, _)| {
                Some(*field) != focused
                    && page.draft.rejected(&field.field()).is_none()
                    && !page.logs_paths.typed_while_saving.contains(field)
            })
            .filter_map(|(field, input)| {
                let want = field.text(&shown);
                (input.read(cx).value() != want).then(|| (input.clone(), want))
            })
            .collect();
        for (input, want) in stale {
            input.update(cx, |state, cx| state.set_value(want, window, cx));
        }
    }

    /// The text in `field`'s input, once Settings has made it.
    #[cfg(test)]
    pub(in crate::ui) fn settings_field_text(&self, field: TextField, cx: &App) -> Option<String> {
        let input = self.settings_page.logs_paths.input(field)?;
        Some(input.read(cx).value().to_string())
    }

    /// Types `text` into `field`'s input, as the keyboard would, even while a Save is being
    /// written (which the palette's edits wait for).
    #[cfg(test)]
    pub(in crate::ui) fn type_settings_text(
        &mut self,
        field: TextField,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(input) = self.settings_page.logs_paths.input(field).cloned() {
            input.update(cx, |state, cx| state.set_value(text.to_owned(), window, cx));
        }
        self.apply_settings_text(field, text, cx);
    }

    /// Whether a Save is being written.
    #[cfg(test)]
    pub(in crate::ui) fn settings_saving(&self) -> bool {
        self.settings_page.saving.is_some()
    }

    /// Sets `field` to `text`, as typing it would: the palette's and automation's way in.
    fn set_settings_text(
        &mut self,
        field: TextField,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(input) = self.settings_page.logs_paths.input(field).cloned() {
            input.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
        }
        self.apply_settings_text(field, &text, cx);
    }

    /// `text` is in `field` now: a valid value is an edit, an invalid one an error under the
    /// field that keeps Save from writing. An invalid value drops the field's earlier edit, so
    /// nothing saves what the field no longer shows.
    fn apply_settings_text(&mut self, field: TextField, text: &str, cx: &mut Context<Self>) {
        if !self.settings_page.open {
            return;
        }
        // Not over a Save being written: read again once it is answered.
        if self.settings_page.saving.is_some() {
            self.settings_page
                .logs_paths
                .typed_while_saving
                .insert(field);
            return;
        }
        self.record_settings_text(field, text, cx);
    }

    /// [`Self::apply_settings_text`] once no Save is being written, whether or not Settings is
    /// still open.
    fn record_settings_text(&mut self, field: TextField, text: &str, cx: &mut Context<Self>) {
        let file = self.model.read(cx).settings.clone();
        self.record_settings_text_over(field, text, Some(&file), cx);
    }

    /// [`Self::record_settings_text`] with the value compared against `holding` (see
    /// [`super::super::settings_draft::SettingsDraft::edit_over`]).
    fn record_settings_text_over(
        &mut self,
        field: TextField,
        text: &str,
        holding: Option<&Settings>,
        cx: &mut Context<Self>,
    ) {
        let file = self.model.read(cx).settings.clone();
        let page = &mut self.settings_page;
        match field.parse(text) {
            // An accepted value replaces one rejected before.
            Ok(change) => page.draft.edit_over(change, holding, &file),
            Err(why) => page
                .draft
                .reject(field.field(), text.to_owned(), why, &file),
        }
        page.save_note = None;
        cx.notify();
    }

    /// Puts the text typed into fields while a Save was being written into the draft, now that
    /// the Save is answered (or given up on): before the written edits leave the draft, so a
    /// newer value typed over one stays, and before a quit or close waiting on the Save goes on.
    /// `saved` is the file as the core saved it, which the view may not have heard of yet: each
    /// value is compared with it. `None`: what the file holds is not known (the Save failed or
    /// went unanswered), and each value typed is kept as an edit.
    pub(super) fn take_typed_while_saving(
        &mut self,
        saved: Option<&Settings>,
        cx: &mut Context<Self>,
    ) {
        let typed = std::mem::take(&mut self.settings_page.logs_paths.typed_while_saving);
        for field in typed {
            if let Some(input) = self.settings_page.logs_paths.input(field) {
                let text = input.read(cx).value().to_string();
                self.record_settings_text_over(field, &text, saved, cx);
            }
        }
    }

    /// Shows the first value rejected: its section and, for a field of these sections, the
    /// keyboard cursor and focus on it, scrolled into view. What Save does while one is.
    pub(super) fn show_settings_field_error(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = &self.settings_page.draft;
        let Some(rejected) = draft
            .touched()
            .find(|field| draft.rejected(field).is_some())
            .cloned()
        else {
            return;
        };
        if let Some(field) = TextField::ALL.into_iter().find(|f| f.field() == rejected) {
            self.settings_page.section = field.section();
            self.settings_page.cursor = Some(Stop::Text(field));
            self.scroll_to_settings_cursor(cx);
            self.focus_settings_stop(window, cx);
        } else if let Some(section) = Section::ALL.into_iter().find(|s| s.holds(&rejected)) {
            self.show_settings_section(section, window, cx);
        }
        cx.notify();
    }

    /// How many files the Paths section lists, as shown.
    pub(super) fn settings_kubeconfigs(&self, cx: &App) -> usize {
        self.settings_shown(cx).kubeconfig_paths.len()
    }

    /// Scrolls the section so the control under the keyboard cursor is in view.
    pub(super) fn scroll_to_settings_cursor(&mut self, cx: &App) {
        let Some(stop) = self.settings_page.cursor else {
            return;
        };
        if let Some(row) = content_row(stop, self.settings_kubeconfigs(cx)) {
            self.settings_page.scroll.scroll_to_item(row);
        }
    }

    /// Adds `paths` to the kubeconfig list in the draft, each that can be: one already listed,
    /// or not a file, is left out and the list says why.
    fn add_kubeconfig_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if !self.settings_page.open {
            return;
        }
        if self.settings_page.saving.is_some() {
            self.settings_page.logs_paths.kubeconfig_note = Some(
                "Settings were being saved, so nothing was added. Add the files again.".into(),
            );
            cx.notify();
            return;
        }
        // The files are looked at off the UI thread: a slow or gone network mount must not
        // stall the window.
        let home = home();
        let before = self.settings_shown(cx).kubeconfig_paths;
        let checked = cx.background_spawn({
            let before = before.clone();
            async move {
                let mut list = before;
                let mut note = None;
                for path in paths {
                    match with_kubeconfig(&list, &path, home.as_deref()) {
                        Ok(added) => list = added,
                        Err(why) => note = Some(why),
                    }
                }
                (list, note)
            }
        });
        cx.spawn(async move |this, cx| {
            let (list, note) = checked.await;
            let _ = this.update(cx, |ws, cx| {
                // Applied only over the list it was checked against.
                let current = ws.settings_shown(cx).kubeconfig_paths;
                if !ws.settings_page.open || ws.settings_page.saving.is_some() || current != before
                {
                    ws.settings_page.logs_paths.kubeconfig_note = Some(
                        "The list changed while the files were checked, so nothing was added. \
                         Add them again."
                            .into(),
                    );
                    cx.notify();
                    return;
                }
                if list != before {
                    ws.edit_settings(Change::KubeconfigPaths(list), cx);
                }
                ws.settings_page.logs_paths.kubeconfig_note = note;
                cx.notify();
            });
        })
        .detach();
    }

    /// Drops `path` from the kubeconfig list in the draft: the row at `index` when it holds
    /// `path` (a file listed twice keeps its other row, and the order of the rest), otherwise
    /// the first row that does.
    fn remove_kubeconfig_path(
        &mut self,
        path: &std::path::Path,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if !self.settings_page.open || self.settings_page.saving.is_some() {
            return;
        }
        let mut list = self.settings_shown(cx).kubeconfig_paths;
        let at = index
            .filter(|&i| list.get(i).is_some_and(|p| p == path))
            .or_else(|| list.iter().position(|p| p == path));
        let Some(at) = at else {
            self.settings_page.logs_paths.kubeconfig_note =
                Some(format!("{} is not in the list.", path.display()));
            cx.notify();
            return;
        };
        list.remove(at);
        // The cursor stays in the list, on the row that took this one's place.
        if let Some(Stop::RemoveKubeconfig(index)) = self.settings_page.cursor
            && index >= list.len()
        {
            self.settings_page.cursor = Some(match list.len() {
                0 => Stop::AddKubeconfig,
                len => Stop::RemoveKubeconfig(len - 1),
            });
        }
        self.settings_page.logs_paths.kubeconfig_note = None;
        self.edit_settings(Change::KubeconfigPaths(list), cx);
    }

    /// Opens the file picker; the files chosen are added to the list.
    fn choose_kubeconfig_paths(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Add".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = chosen.await;
            let _ = this.update(cx, |ws, cx| match result {
                Ok(Ok(Some(paths))) => ws.add_kubeconfig_paths(paths, cx),
                Ok(Err(e)) => {
                    ws.settings_page.logs_paths.kubeconfig_note =
                        Some(format!("Could not open the file picker: {e}"));
                    cx.notify();
                }
                // Cancelled.
                _ => {}
            });
        })
        .detach();
    }

    /// Enter or Space on a control of these sections.
    pub(super) fn activate_logs_paths_stop(
        &mut self,
        stop: Stop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match stop {
            Stop::Text(_) => self.focus_settings_stop(window, cx),
            Stop::RemoveKubeconfig(index) => {
                if let Some(path) = self.settings_shown(cx).kubeconfig_paths.get(index) {
                    self.remove_kubeconfig_path(&path.clone(), Some(index), cx);
                }
            }
            Stop::AddKubeconfig => self.choose_kubeconfig_paths(cx),
            Stop::ProtectLocalClusters => {
                let on = self.settings_shown(cx).protect_local_clusters;
                self.edit_settings(protect(!on), cx);
            }
            _ => {}
        }
    }

    /// Focus follows the keyboard cursor into a text field, and back out to the view from one.
    pub(super) fn focus_settings_stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = match self.settings_page.cursor {
            Some(Stop::Text(field)) => self.settings_page.logs_paths.input(field).cloned(),
            _ => None,
        };
        match input {
            Some(input) => input.update(cx, |state, cx| state.focus(window, cx)),
            None => {
                if self.settings_page.logs_paths.focused(window, cx).is_some() {
                    window.focus(&self.settings_page.focus, cx);
                }
            }
        }
    }

    /// Asks before a Save that turns local cluster protection off. Confirmed, it saves the draft
    /// it was asked about, over the file it was asked over; if either changed meanwhile, it saves
    /// nothing and says so. Cancelled, nothing is saved and `then` (quit, close) does not run.
    pub(super) fn confirm_unprotect(
        &mut self,
        then: Option<Then>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edits = self.settings_page.draft.edits();
        let file = self.model.read(cx).settings.clone();
        let on_disk = settings_view::banner_shown(&self.settings_page, &file);
        let text = DialogText {
            title: UNPROTECT_TITLE.into(),
            text: if on_disk {
                format!("{UNPROTECT_TEXT}{UNPROTECT_ON_DISK}")
            } else {
                UNPROTECT_TEXT.into()
            },
            ok: "Turn Off and Save".into(),
            cancel: "Cancel".into(),
            id: "settings-unprotect-description",
            // Loosening the write gate takes the button: Enter cancels.
            default_cancel: true,
        };
        let workspace = cx.weak_entity();
        confirm::open(window, cx, text, move |window, cx| {
            let then = then.clone();
            let _ = workspace.update(cx, |ws, cx| {
                ws.confirmed_unprotect(&edits, &file, then, window, cx)
            });
        });
    }

    fn confirmed_unprotect(
        &mut self,
        edits: &[Change],
        file: &Arc<Settings>,
        then: Option<Then>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = &self.settings_page;
        let current = self.model.read(cx).settings.clone();
        let unchanged = page.draft.edits() == edits && *current == **file;
        if !unchanged || !page.draft.loosens_protection() || !self.can_save_settings(cx) {
            self.settings_page.save_note = Some(UNPROTECT_STALE.to_owned());
            cx.notify();
            return;
        }
        // The core turns protection off only over the file confirmed here: if `settings.toml`
        // changed since, it refuses, and the confirmation is asked again.
        let confirmed = SettingsRevision::of(file);
        self.write_settings_draft(then, Some(confirmed), window, cx);
    }
}
