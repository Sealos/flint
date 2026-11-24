//! The Settings view (#739, #740): Flint's preferences in the main area, with a section list on
//! the left. Edits form a [`SettingsDraft`] that waits for Save; Revert drops it. The draft
//! outlives switching views and clusters until the app quits, and the sidebar's Settings entry
//! shows a dot while it holds edits.
//!
//! Opened from Flint > Settings…, `secondary-,` and the command palette. Every control is a
//! registered action, so the palette, keys and automation reach the same handlers
//! ([`super::workspace`]'s `workspace_settings.rs`).
//!
//! Sections so far: Appearance, Alerts (desktop notifications) and Watching (the default watch
//! mode). A slice that adds settings adds a [`Section`] (or a block to one) and its
//! [`super::settings_draft::Change`]s.

// module-tests: settings_view_tests.rs

use flint_core::{Appearance, Settings, WatchMode};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{
    Disableable as _, Selectable as _, Sizable as _, StyledExt as _, Theme, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use super::registry;
use super::settings_draft::{Field, SettingsDraft};
#[cfg(any(test, feature = "automation"))]
use super::view_state::SettingsState;

/// The key context of the Settings view, outside any text field.
pub const SETTINGS_KEYS: &str = "SettingsView && !Input";

actions!(
    flint,
    [
        /// Open Settings in the main area.
        OpenSettings
    ]
);
actions!(
    settings,
    [
        /// Leave Settings for the view shown before. The draft stays.
        CloseSettings,
        /// Write the edited settings to `settings.toml`, and only those.
        SaveSettings,
        /// Drop the unsaved edits and show `settings.toml` as it is.
        RevertSettings
    ]
);

/// Show one of the Settings sections.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct ShowSettingsSection {
    pub section: Section,
}

/// Set the appearance in the Settings draft.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditAppearance {
    pub appearance: Appearance,
}

/// Turn desktop notifications on or off in the Settings draft.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditDesktopNotifications {
    pub enabled: bool,
}

/// Set the default watch mode in the Settings draft.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditWatchMode {
    pub mode: WatchMode,
}

/// A section of the Settings view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    #[default]
    Appearance,
    Alerts,
    Watching,
}

impl Section {
    pub const ALL: [Section; 3] = [Section::Appearance, Section::Alerts, Section::Watching];

    pub fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Alerts => "Alerts",
            Section::Watching => "Watching",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Section::Appearance => "settings-section-appearance",
            Section::Alerts => "settings-section-alerts",
            Section::Watching => "settings-section-watching",
        }
    }

    /// The fields its controls edit.
    pub fn fields(self) -> &'static [Field] {
        match self {
            Section::Appearance => &[Field::Appearance],
            Section::Alerts => &[Field::DesktopNotifications],
            Section::Watching => &[Field::WatchMode],
        }
    }
}

/// What the main area would show without Settings, when it was opened: once that changes (a
/// cluster, a kind, an object or a set opened meanwhile), Settings gives way to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainKey {
    pub cluster: Option<flint_core::ClusterId>,
    /// The running set's list.
    pub set: Option<EntityId>,
}

/// What the Settings view holds while the window is open: whether it is shown, the section,
/// and the draft, which outlives leaving the view.
pub struct SettingsPage {
    pub open: bool,
    pub section: Section,
    pub draft: SettingsDraft,
    /// The view's own focus, for its keys.
    pub focus: FocusHandle,
    /// What the main area showed when Settings opened.
    pub opened_over: Option<MainKey>,
    /// Waits for the core to report a Save written (or failed), and gives up after a while.
    pub saving: Option<(Subscription, Task<()>)>,
}

impl SettingsPage {
    pub fn new(cx: &mut App) -> Self {
        Self {
            open: false,
            section: Section::default(),
            draft: SettingsDraft::default(),
            focus: cx.focus_handle(),
            opened_over: None,
            saving: None,
        }
    }

    /// The view as automation reads it, over `file` (`invalid`: it could not be read).
    #[cfg(any(test, feature = "automation"))]
    pub fn state(&self, file: &Settings, invalid: bool) -> SettingsState {
        let shown = self.draft.shown(file);
        SettingsState {
            open: self.open,
            section: self.section,
            dirty: self.draft.is_dirty(),
            edited: self.draft.edited().map(Field::key).collect(),
            appearance: appearance_name(shown.appearance),
            desktop_notifications: shown.desktop_notifications,
            watch_mode: watch_mode_name(shown.watch_mode),
            changed_on_disk: self.draft.changed_on_disk(file),
            invalid,
            can_save: self.saving.is_none() && can_save(&self.draft, invalid),
        }
    }
}

/// Save needs edits and a readable file.
pub fn can_save(draft: &SettingsDraft, invalid: bool) -> bool {
    draft.is_dirty() && !invalid
}

#[cfg(any(test, feature = "automation"))]
pub fn appearance_name(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::System => "system",
        Appearance::Light => "light",
        Appearance::Dark => "dark",
    }
}

#[cfg(any(test, feature = "automation"))]
pub fn watch_mode_name(mode: WatchMode) -> &'static str {
    match mode {
        WatchMode::All => "all",
        WatchMode::OnView => "on_view",
    }
}

/// The banner over a pending draft whose file changed meanwhile.
pub const CHANGED_ON_DISK: &str = "settings.toml was modified outside Settings. Reload shows the \
    file and drops your edits; Save anyway writes only the settings you edited.";

/// The banner while the file cannot be read.
pub const INVALID: &str =
    "settings.toml is invalid. Fix it to save settings; until then Flint uses safe defaults.";

/// The sidebar's entry: Settings, with a dot while the draft holds edits.
pub fn render_entry(page: &SettingsPage, theme: &Theme) -> impl IntoElement {
    let dirty = page.draft.is_dirty();
    let label = if dirty {
        "Settings (unsaved changes)"
    } else {
        "Settings"
    };
    h_flex()
        .id("settings-entry")
        .flex_none()
        .w_full()
        .px_2()
        .py_1()
        .border_t_1()
        .border_color(theme.sidebar_border)
        .aria_label(label)
        .child(
            Button::new("open-settings")
                .xsmall()
                .ghost()
                .w_full()
                .label("Settings")
                .selected(page.open)
                .tooltip(format!(
                    "Settings ({})",
                    registry::key_hint(OPEN_SETTINGS_KEY)
                ))
                .on_click(registry::on_click(OpenSettings)),
        )
        .when(dirty, |el| {
            el.child(
                super::theme::status_dot(theme.foreground, true)
                    .id("settings-entry-dot")
                    .flex_none()
                    .test_support(),
            )
        })
        .test_support()
}

/// The key that opens Settings: Cmd-, on macOS, Ctrl-, on Linux.
pub const OPEN_SETTINGS_KEY: &str = "secondary-,";

/// The Settings view, over `file` as the core last reported it.
pub fn render(page: &SettingsPage, file: &Settings, invalid: bool, theme: &Theme) -> AnyElement {
    let draft = &page.draft;
    let shown = draft.shown(file);
    let changed = draft.changed_on_disk(file);
    v_flex()
        .id("settings-view")
        .key_context("SettingsView")
        .track_focus(&page.focus)
        .flex_1()
        .min_w_0()
        .h_full()
        .child(header(theme))
        .when(invalid, |el| {
            el.child(banner(
                "settings-invalid",
                INVALID,
                theme.danger,
                None,
                theme,
            ))
        })
        .when(changed, |el| {
            let buttons = h_flex()
                .flex_none()
                .gap_1()
                .child(
                    Button::new("settings-reload")
                        .xsmall()
                        .label("Reload")
                        .on_click(registry::on_click(RevertSettings)),
                )
                .child(
                    Button::new("settings-save-anyway")
                        .xsmall()
                        .label("Save anyway")
                        .disabled(invalid)
                        .on_click(registry::on_click(SaveSettings)),
                );
            el.child(banner(
                "settings-changed-on-disk",
                CHANGED_ON_DISK,
                theme.warning,
                Some(buttons.into_any_element()),
                theme,
            ))
        })
        .child(
            h_flex()
                .flex_1()
                .min_h_0()
                .child(section_list(page, theme))
                .child(
                    v_flex()
                        .id("settings-content")
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_y_scroll()
                        .p_4()
                        .gap_3()
                        .child(section_body(page.section, &shown, draft, theme)),
                ),
        )
        .child(footer(draft, invalid, page.saving.is_some(), theme))
        .test_support()
        .into_any_element()
}

fn header(theme: &Theme) -> impl IntoElement {
    super::theme::moves_window(
        h_flex()
            .id("settings-header")
            .flex_none()
            .h(px(super::workspace::HEADER_HEIGHT))
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border),
    )
    .child(div().flex_1().text_sm().font_semibold().child("Settings"))
    .child(
        Button::new("settings-close")
            .xsmall()
            .ghost()
            .label("Done")
            .tooltip("Leave Settings. Unsaved edits stay until you save or revert them.")
            .on_click(registry::on_click(CloseSettings)),
    )
}

fn banner(
    id: &'static str,
    text: &'static str,
    color: Hsla,
    buttons: Option<AnyElement>,
    theme: &Theme,
) -> impl IntoElement {
    h_flex()
        .id(id)
        .aria_label(text)
        .flex_none()
        .gap_3()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(color.opacity(0.1))
        .text_xs()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(theme.foreground)
                .child(text),
        )
        .children(buttons)
        .test_support()
}

fn section_list(page: &SettingsPage, theme: &Theme) -> impl IntoElement {
    v_flex()
        .id("settings-sections")
        .flex_none()
        .w(px(160.))
        .h_full()
        .gap_px()
        .p_2()
        .border_r_1()
        .border_color(theme.border)
        .children(Section::ALL.map(|section| {
            let edited = section.fields().iter().any(|f| page.draft.is_edited(*f));
            h_flex()
                .w_full()
                .gap_1()
                .child(
                    Button::new(section.id())
                        .xsmall()
                        .ghost()
                        .flex_1()
                        .label(section.label())
                        .selected(page.section == section)
                        .on_click(registry::on_click(ShowSettingsSection { section })),
                )
                .when(edited, |el| {
                    el.child(super::theme::status_dot(theme.foreground, true).flex_none())
                })
        }))
}

fn section_body(
    section: Section,
    shown: &Settings,
    draft: &SettingsDraft,
    theme: &Theme,
) -> AnyElement {
    match section {
        Section::Appearance => group(
            "Appearance",
            "Flint Light, Flint Dark, or whichever the system uses. View > Appearance changes it at \
             once; here it changes on Save.",
            draft.is_edited(Field::Appearance),
            theme,
        )
        .child(
            h_flex().gap_1().children(
                [
                    (Appearance::System, "System", "settings-appearance-system"),
                    (Appearance::Light, "Light", "settings-appearance-light"),
                    (Appearance::Dark, "Dark", "settings-appearance-dark"),
                ]
                .map(|(appearance, label, id)| {
                    Button::new(id)
                        .small()
                        .label(label)
                        .selected(shown.appearance == appearance)
                        .on_click(registry::on_click(EditAppearance { appearance }))
                }),
            ),
        )
        .into_any_element(),
        Section::Alerts => group(
            "Desktop notifications",
            "Post a notification when a watched cluster has a new problem. Only clusters that \
             watch all kinds are covered. Per-cluster choices stay in the Alerts menu.",
            draft.is_edited(Field::DesktopNotifications),
            theme,
        )
        .child(
            Checkbox::new("settings-desktop-notifications")
                .label("Desktop notifications")
                .checked(shown.desktop_notifications)
                .on_click(registry::on_click(EditDesktopNotifications {
                    enabled: !shown.desktop_notifications,
                })),
        )
        .into_any_element(),
        Section::Watching => group(
            "Default watch mode",
            "Which kinds are watched on the active cluster. All kinds keeps the overview, \
             problems and alerts live; Open views only watches a kind while a view shows it. A \
             cluster's own choice, made from its header, overrides this.",
            draft.is_edited(Field::WatchMode),
            theme,
        )
        .child(
            h_flex().gap_1().children(
                [
                    (WatchMode::All, "All kinds", "settings-watch-all"),
                    (WatchMode::OnView, "Open views only", "settings-watch-on-view"),
                ]
                .map(|(mode, label, id)| {
                    Button::new(id)
                        .small()
                        .label(label)
                        .selected(shown.watch_mode == mode)
                        .on_click(registry::on_click(EditWatchMode { mode }))
                }),
            ),
        )
        .into_any_element(),
    }
}

/// A titled block of controls, saying when it holds an unsaved edit.
fn group(title: &'static str, text: &'static str, edited: bool, theme: &Theme) -> Div {
    v_flex()
        .gap_2()
        .max_w(px(560.))
        .child(
            h_flex()
                .gap_2()
                .child(div().text_sm().font_semibold().child(title))
                .when(edited, |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Edited"),
                    )
                }),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text),
        )
}

fn footer(draft: &SettingsDraft, invalid: bool, saving: bool, theme: &Theme) -> impl IntoElement {
    let dirty = draft.is_dirty();
    let note = if saving {
        "Saving…"
    } else if dirty {
        "Unsaved changes"
    } else {
        "No unsaved changes"
    };
    h_flex()
        .id("settings-footer")
        .flex_none()
        .gap_2()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(theme.border)
        .child(
            div()
                .id("settings-draft-note")
                .flex_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .aria_label(note)
                .child(note)
                .test_support(),
        )
        .child(
            Button::new("settings-revert")
                .small()
                .label("Revert")
                .disabled(!dirty)
                .on_click(registry::on_click(RevertSettings)),
        )
        .child(
            Button::new("settings-save")
                .small()
                .primary()
                .label("Save")
                .disabled(saving || !can_save(draft, invalid))
                .on_click(registry::on_click(SaveSettings)),
        )
}
