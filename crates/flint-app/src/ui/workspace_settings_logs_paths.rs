//! The workspace's side of the Logs, Paths and Write safety sections of Settings
//! ([`settings_logs_paths`]): their actions and guards, the text fields, the kubeconfig file
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
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;

use super::super::confirm::{self, DialogText};
use super::super::registry::{Bind, Guards};
use super::super::settings_draft::Change;
use super::super::settings_logs_paths::{
    AddKubeconfigPath, ChooseKubeconfigPaths, EditHelmPath, EditLogBufferLines, EditLogBufferMib,
    EditLogTotalMib, EditPathOverride, EditProtectLocalClusters, Inputs, RemoveKubeconfigPath,
    TextField, UNPROTECT_ON_DISK, UNPROTECT_STALE, UNPROTECT_TEXT, UNPROTECT_TITLE,
    with_kubeconfig,
};
use super::super::settings_view::{self, Stop, Then};
use super::Workspace;

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
            ws.remove_kubeconfig_path(&PathBuf::from(&action.path), cx)
        })
        .bind::<ChooseKubeconfigPaths>(|ws, _, _, cx| ws.choose_kubeconfig_paths(cx))
        .bind::<EditProtectLocalClusters>(|ws, action, _, cx| {
            ws.edit_settings(Change::ProtectLocalClusters(action.enabled), cx)
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
        if self.settings_page.saving.is_none() {
            let typed = std::mem::take(&mut self.settings_page.logs_paths.typed_while_saving);
            for field in typed {
                if let Some(input) = self.settings_page.logs_paths.input(field) {
                    let text = input.read(cx).value().to_string();
                    self.apply_settings_text(field, &text, cx);
                }
            }
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
                    && !page.logs_paths.errors.contains_key(field)
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
    /// field that keeps Save disabled. An invalid value drops the field's earlier edit, so
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
        match field.parse(text, home().as_deref()) {
            Ok(change) => {
                self.settings_page.logs_paths.errors.remove(&field);
                self.edit_settings(change, cx);
            }
            Err(why) => {
                self.settings_page.logs_paths.errors.insert(field, why);
                self.settings_page.draft.forget(field.field());
                self.settings_page.save_note = None;
                cx.notify();
            }
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
        let home = home();
        let before = self.settings_shown(cx).kubeconfig_paths;
        let mut list = before.clone();
        let mut note = None;
        for path in paths {
            match with_kubeconfig(&list, &path, home.as_deref()) {
                Ok(added) => list = added,
                Err(why) => note = Some(why),
            }
        }
        if list != before {
            self.edit_settings(Change::KubeconfigPaths(list), cx);
        }
        self.settings_page.logs_paths.kubeconfig_note = note;
        cx.notify();
    }

    /// Drops `path` from the kubeconfig list in the draft.
    fn remove_kubeconfig_path(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        if !self.settings_page.open || self.settings_page.saving.is_some() {
            return;
        }
        let mut list = self.settings_shown(cx).kubeconfig_paths;
        let Some(at) = list.iter().position(|p| p == path) else {
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
                    self.remove_kubeconfig_path(&path.clone(), cx);
                }
            }
            Stop::AddKubeconfig => self.choose_kubeconfig_paths(cx),
            Stop::ProtectLocalClusters => {
                let on = self.settings_shown(cx).protect_local_clusters;
                self.edit_settings(Change::ProtectLocalClusters(!on), cx);
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
        self.write_settings_draft(then, window, cx);
    }
}
