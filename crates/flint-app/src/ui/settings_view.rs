//! The Settings view (#739, #740, #854): Flint's preferences in a modal over the main window,
//! with a section list on the left. Edits form a [`SettingsDraft`] that waits for Save; Revert
//! drops it. Closing the modal with edits asks Discard / Keep editing, so the draft never
//! outlives it.
//!
//! Opened from Flint > Settings…, `secondary-,`, the sidebar's Settings entry and the command
//! palette. Every control is a registered action, so the palette, keys and automation reach the
//! same handlers ([`super::workspace`]'s `workspace_settings.rs`). From the keyboard, Tab and
//! Shift-Tab (or the arrows) go round the view's own controls ([`stops`]), Enter or Space works
//! the one under the cursor, and Esc closes.
//!
//! Sections so far: Appearance, Alerts (desktop notifications), Watching (the default watch
//! mode), Thresholds and Cluster overrides ([`super::settings_thresholds`]), and Logs, Paths and
//! Write safety ([`super::settings_logs_paths`]), and Network ([`super::settings_network`]), whose
//! proxy is applied on its own rather than saved with the draft. A slice that adds settings adds a [`Section`]
//! (or a block to one), its
//! [`super::settings_draft::Change`]s and its [`Stop`]s.

// module-tests: settings_view_tests.rs

use std::rc::Rc;

use flint_core::{Appearance, Settings, WatchMode};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{
    Disableable as _, IconName, Selectable as _, Sizable as _, StyledExt as _, Theme, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use super::registry;
use super::settings_draft::{Field, SettingsDraft};
use super::settings_logs_paths::{self as logs_paths, LogsPathsFields, TextField};
use super::settings_network::{self as network, NetworkFields, NetworkStop};
use super::settings_thresholds::{
    self, KnownCluster, OverridesLayout, ThresholdStop, ThresholdsPage,
};
#[cfg(any(test, feature = "automation"))]
use super::view_state::SettingsState;

/// The key context of the Settings view, outside any text field.
pub const SETTINGS_KEYS: &str = "SettingsView && !Input";

/// The key context of a text field in the Settings view.
pub const SETTINGS_INPUT_KEYS: &str = "SettingsView > Input";

actions!(
    flint,
    [
        /// Open Settings over the main window.
        OpenSettings
    ]
);
actions!(
    settings,
    [
        /// Close Settings, asking first whether to discard unsaved edits.
        CloseSettings,
        /// Write the edited settings to `settings.toml`, and only those.
        SaveSettings,
        /// Drop the unsaved edits and show `settings.toml` as it is.
        RevertSettings,
        /// Move the keyboard cursor to the next control in Settings, round the view.
        NextSettingsControl,
        /// Move the keyboard cursor to the previous control in Settings, round the view.
        PreviousSettingsControl,
        /// Work the control under the keyboard cursor, as a click does.
        ActivateSettingsControl
    ]
);

/// Show one of the Settings sections, opening Settings if it is closed.
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
    Thresholds,
    ClusterOverrides,
    Logs,
    Paths,
    Safety,
    Network,
}

impl Section {
    pub const ALL: [Section; 9] = [
        Section::Appearance,
        Section::Alerts,
        Section::Watching,
        Section::Thresholds,
        Section::ClusterOverrides,
        Section::Logs,
        Section::Paths,
        Section::Safety,
        Section::Network,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Alerts => "Alerts",
            Section::Watching => "Watching",
            Section::Thresholds => "Thresholds",
            Section::ClusterOverrides => "Cluster overrides",
            Section::Logs => "Logs",
            Section::Paths => "Paths",
            Section::Safety => "Write safety",
            Section::Network => "Network",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Section::Appearance => "settings-section-appearance",
            Section::Alerts => "settings-section-alerts",
            Section::Watching => "settings-section-watching",
            Section::Thresholds => "settings-section-thresholds",
            Section::ClusterOverrides => "settings-section-cluster-overrides",
            Section::Logs => "settings-section-logs",
            Section::Paths => "settings-section-paths",
            Section::Safety => "settings-section-safety",
            Section::Network => "settings-section-network",
        }
    }

    /// Whether its controls edit `field`.
    pub fn holds(self, field: &Field) -> bool {
        match self {
            Section::Appearance => *field == Field::Appearance,
            Section::Alerts => *field == Field::DesktopNotifications,
            Section::Watching => *field == Field::WatchMode,
            Section::Thresholds => settings_thresholds::holds(field, false),
            Section::ClusterOverrides => settings_thresholds::holds(field, true),
            Section::Logs => matches!(
                field,
                Field::LogBufferLines | Field::LogBufferMib | Field::LogTotalMib
            ),
            Section::Paths => matches!(field, Field::KubeconfigPaths | Field::Path | Field::Helm),
            Section::Safety => *field == Field::ProtectLocalClusters,
            // The proxy is applied on its own, never through the draft.
            Section::Network => false,
        }
    }
}

/// A control the keyboard cursor can stand on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The header's close button.
    Close,
    Reload,
    SaveAnyway,
    Section(Section),
    Appearance(Appearance),
    DesktopNotifications,
    WatchMode(WatchMode),
    Threshold(ThresholdStop),
    /// A text field of the Logs or Paths section.
    Text(TextField),
    /// The Remove button of the kubeconfig file at this position.
    RemoveKubeconfig(usize),
    AddKubeconfig,
    ProtectLocalClusters,
    /// A control of the Network section.
    Network(NetworkStop),
    Revert,
    Save,
}

impl Stop {
    /// The element id of the control.
    pub fn id(self) -> SharedString {
        match self {
            Stop::Close => "settings-close".into(),
            Stop::Reload => "settings-reload".into(),
            Stop::SaveAnyway => "settings-save-anyway".into(),
            Stop::Section(section) => section.id().into(),
            Stop::Appearance(Appearance::System) => "settings-appearance-system".into(),
            Stop::Appearance(Appearance::Light) => "settings-appearance-light".into(),
            Stop::Appearance(Appearance::Dark) => "settings-appearance-dark".into(),
            Stop::DesktopNotifications => "settings-desktop-notifications".into(),
            Stop::WatchMode(WatchMode::All) => "settings-watch-all".into(),
            Stop::WatchMode(WatchMode::OnView) => "settings-watch-on-view".into(),
            Stop::Threshold(stop) => stop.id(),
            Stop::Text(field) => field.id().into(),
            Stop::RemoveKubeconfig(index) => logs_paths::remove_id(index).into(),
            Stop::AddKubeconfig => "settings-kubeconfig-add".into(),
            Stop::ProtectLocalClusters => "settings-protect-local-clusters".into(),
            Stop::Network(stop) => stop.id().into(),
            Stop::Revert => "settings-revert".into(),
            Stop::Save => "settings-save".into(),
        }
    }
}

/// The controls in the order Tab visits them, with `section` shown, the on-disk banner shown or
/// not, Cluster overrides drawn as `overrides` says, and `kubeconfigs` files in the Paths list.
pub fn stops(
    section: Section,
    banner: bool,
    overrides: OverridesLayout,
    kubeconfigs: usize,
) -> Vec<Stop> {
    let mut stops = vec![Stop::Close];
    if banner {
        stops.extend([Stop::Reload, Stop::SaveAnyway]);
    }
    stops.extend(Section::ALL.map(Stop::Section));
    match section {
        Section::Appearance => stops.extend(
            [Appearance::System, Appearance::Light, Appearance::Dark].map(Stop::Appearance),
        ),
        Section::Alerts => stops.push(Stop::DesktopNotifications),
        Section::Watching => stops.extend([WatchMode::All, WatchMode::OnView].map(Stop::WatchMode)),
        Section::Thresholds => stops.extend(
            settings_thresholds::global_stops()
                .into_iter()
                .map(Stop::Threshold),
        ),
        Section::ClusterOverrides => stops.extend(
            settings_thresholds::override_stops(overrides)
                .into_iter()
                .map(Stop::Threshold),
        ),
        Section::Logs => stops.extend(logs_paths::logs_stops()),
        Section::Paths => stops.extend(logs_paths::paths_stops(kubeconfigs)),
        Section::Safety => stops.push(Stop::ProtectLocalClusters),
        Section::Network => stops.extend(network::stops()),
    }
    stops.extend([Stop::Revert, Stop::Save]);
    stops
}

/// What closing or quitting does once the settings question is answered.
pub type Then = Rc<dyn Fn(&mut Window, &mut App)>;

/// A Save handed to the core, until it answers.
pub struct Saving {
    /// Closing or quitting that waits for it.
    pub after: Vec<Then>,
    pub _answer: Task<()>,
    pub _give_up: Task<()>,
}

/// What the Settings view holds while the window is open: whether the modal is open, the
/// section, and the draft, which is empty whenever the modal is closed.
pub struct SettingsPage {
    /// Whether the modal is open.
    pub open: bool,
    pub section: Section,
    pub draft: SettingsDraft,
    /// The view's own focus, for its keys.
    pub focus: FocusHandle,
    /// A Save the core has not answered yet. Its edits stay in the draft meanwhile.
    pub saving: Option<Saving>,
    /// Why the last Save did not go through, until the next edit, Save or Revert.
    pub save_note: Option<String>,
    /// The control the keyboard cursor stands on, once Tab moved it.
    pub cursor: Option<Stop>,
    /// Thresholds and Cluster overrides: rows added, the cluster list, the field being typed.
    pub thresholds: ThresholdsPage,
    /// The Logs and Paths sections' text inputs and the kubeconfig list's note.
    pub logs_paths: LogsPathsFields,
    /// The Network section's proxy fields and the last Apply.
    pub network: NetworkFields,
    /// The scrolling content, so the keyboard cursor can bring a control into view.
    pub scroll: ScrollHandle,
}

impl SettingsPage {
    pub fn new(cx: &mut App) -> Self {
        Self {
            open: false,
            section: Section::default(),
            draft: SettingsDraft::default(),
            focus: cx.focus_handle(),
            saving: None,
            save_note: None,
            cursor: None,
            thresholds: ThresholdsPage::default(),
            logs_paths: LogsPathsFields::default(),
            network: NetworkFields::default(),
            scroll: ScrollHandle::new(),
        }
    }

    /// The view as automation reads it, over `file` (`invalid`: it could not be read), with
    /// the kubeconfig's `clusters`.
    #[cfg(any(test, feature = "automation"))]
    pub fn state(
        &self,
        file: &Settings,
        invalid: bool,
        clusters: &[KnownCluster],
    ) -> SettingsState {
        let shown = self.draft.shown(file);
        SettingsState {
            open: self.open,
            section: self.section,
            dirty: self.draft.is_dirty(),
            edited: self.draft.edited().map(Field::key).collect(),
            appearance: appearance_name(shown.appearance),
            desktop_notifications: shown.desktop_notifications,
            watch_mode: watch_mode_name(shown.watch_mode),
            log_buffer_lines: shown.log_buffer_lines,
            log_buffer_mib: shown.log_buffer_mib,
            log_total_mib: shown.log_total_mib,
            kubeconfig_paths: shown
                .kubeconfig_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            path: shown.path.clone(),
            helm: shown.helm.as_ref().map(|p| p.display().to_string()),
            protect_local_clusters: shown.protect_local_clusters,
            field_errors: TextField::ALL
                .into_iter()
                .filter_map(|field| {
                    let rejected = self.draft.rejected(&field.field())?;
                    Some((field.id(), rejected.why.clone()))
                })
                .collect(),
            kubeconfig_note: self.logs_paths.kubeconfig_note.clone(),
            thresholds: settings_thresholds::view_state(self, &shown, clusters),
            network: network::view_state(&self.network, &file.proxy, invalid),
            changed_on_disk: banner_shown(self, file),
            invalid,
            can_save: self.saving.is_none() && can_save(&self.draft, invalid),
            saving: self.saving.is_some(),
            note: self.save_note.clone(),
            cursor: self.cursor.map(|stop| stop.id().to_string()),
        }
    }
}

/// Save needs edits, none rejected, and a readable file.
pub fn can_save(draft: &SettingsDraft, invalid: bool) -> bool {
    draft.is_ready() && !invalid
}

/// Whether the "modified outside Settings" banner shows: not while a Save of ours is landing.
pub fn banner_shown(page: &SettingsPage, file: &Settings) -> bool {
    page.saving.is_none() && page.draft.changed_on_disk(file)
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

/// The footer while a value typed is not accepted.
pub const REJECTED: &str = "Some values are not accepted. Fix the fields marked in red to save.";

/// The banner while the file cannot be read.
pub const INVALID: &str =
    "settings.toml is invalid. Fix it to save settings; until then Flint uses safe defaults.";

/// The sidebar's entry, which opens Settings. It needs no unsaved-edits dot: closing Settings
/// never leaves edits behind.
pub fn render_entry(page: &SettingsPage, theme: &Theme) -> impl IntoElement {
    h_flex()
        .id("settings-entry")
        .flex_none()
        .w_full()
        .px_2()
        .py_1()
        .border_t_1()
        .border_color(theme.sidebar_border)
        .aria_label("Settings")
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
        .test_support()
}

/// The key that opens Settings: Cmd-, on macOS, Ctrl-, on Linux.
pub const OPEN_SETTINGS_KEY: &str = "secondary-,";

/// `control`, with the focus ring while the keyboard cursor stands on `stop`.
pub(super) fn ring(
    page: &SettingsPage,
    stop: Stop,
    theme: &Theme,
    control: impl IntoElement,
) -> Div {
    let on = page.cursor == Some(stop);
    div()
        .rounded_md()
        .border_2()
        .border_color(if on { theme.ring } else { theme.background })
        .child(control)
}

/// The Settings view, over `file` as the core last reported it, with the kubeconfig's
/// `clusters`: the modal's content, filling it.
pub fn render(
    page: &SettingsPage,
    file: &Settings,
    invalid: bool,
    clusters: &[KnownCluster],
    theme: &Theme,
) -> AnyElement {
    let draft = &page.draft;
    let shown = draft.shown(file);
    let changed = banner_shown(page, file);
    v_flex()
        .id("settings-view")
        .key_context("SettingsView")
        .track_focus(&page.focus)
        .size_full()
        .child(header(page, theme))
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
                .child(ring(
                    page,
                    Stop::Reload,
                    theme,
                    Button::new("settings-reload")
                        .xsmall()
                        .label("Reload")
                        .on_click(registry::on_click(RevertSettings)),
                ))
                .child(ring(
                    page,
                    Stop::SaveAnyway,
                    theme,
                    Button::new("settings-save-anyway")
                        .xsmall()
                        .label("Save anyway")
                        .disabled(invalid || draft.has_rejected())
                        .on_click(registry::on_click(SaveSettings)),
                ));
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
                        .track_scroll(&page.scroll)
                        .p_4()
                        .gap_3()
                        .children(section_body(page, &shown, invalid, clusters, theme)),
                ),
        )
        .child(footer(page, invalid, theme))
        .test_support()
        .into_any_element()
}

fn header(page: &SettingsPage, theme: &Theme) -> impl IntoElement {
    h_flex()
        .id("settings-header")
        .flex_none()
        .h(px(super::workspace::HEADER_HEIGHT))
        .px_3()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .child(div().flex_1().text_sm().font_semibold().child("Settings"))
        .child(ring(
            page,
            Stop::Close,
            theme,
            Button::new("settings-close")
                .accessibility_label("Close")
                .icon(IconName::Close)
                .xsmall()
                .ghost()
                .tooltip("Close Settings (Esc)")
                .on_click(registry::on_click(CloseSettings)),
        ))
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
            let edited = page.draft.touched().any(|f| section.holds(f));
            h_flex()
                .w_full()
                .gap_1()
                .child(
                    ring(
                        page,
                        Stop::Section(section),
                        theme,
                        Button::new(section.id())
                            .xsmall()
                            .ghost()
                            .w_full()
                            .label(section.label())
                            .selected(page.section == section)
                            .on_click(registry::on_click(ShowSettingsSection { section })),
                    )
                    .flex_1(),
                )
                .when(edited, |el| {
                    el.child(super::theme::status_dot(theme.foreground, true).flex_none())
                })
        }))
}

fn section_body(
    page: &SettingsPage,
    shown: &Settings,
    invalid: bool,
    clusters: &[KnownCluster],
    theme: &Theme,
) -> Vec<AnyElement> {
    let draft = &page.draft;
    let one = |element: AnyElement| vec![element];
    match page.section {
        Section::Appearance => one(group(
            "Appearance",
            "Flint Light, Flint Dark, or whichever the system uses. View > Appearance changes it at \
             once; here it changes on Save.",
            draft.is_edited(&Field::Appearance),
            theme,
        )
        .child(
            h_flex().gap_1().children(
                [
                    (Appearance::System, "System"),
                    (Appearance::Light, "Light"),
                    (Appearance::Dark, "Dark"),
                ]
                .map(|(appearance, label)| {
                    let stop = Stop::Appearance(appearance);
                    ring(
                        page,
                        stop,
                        theme,
                        Button::new(stop.id())
                            .small()
                            .label(label)
                            .selected(shown.appearance == appearance)
                            .on_click(registry::on_click(EditAppearance { appearance })),
                    )
                }),
            ),
        )
        .into_any_element()),
        Section::Alerts => one(group(
            "Desktop notifications",
            "Post a notification when a watched cluster has a new problem. Only clusters that \
             watch all kinds are covered. A cluster's own choice (Cluster overrides, or the \
             Alerts menu) overrides this.",
            draft.is_edited(&Field::DesktopNotifications),
            theme,
        )
        .child(
            h_flex().child(ring(
                page,
                Stop::DesktopNotifications,
                theme,
                Checkbox::new(Stop::DesktopNotifications.id())
                    .label("Desktop notifications")
                    .checked(shown.desktop_notifications)
                    .on_click(registry::on_click(EditDesktopNotifications {
                        enabled: !shown.desktop_notifications,
                    })),
            )),
        )
        .into_any_element()),
        Section::Watching => one(group(
            "Default watch mode",
            "Which kinds are watched on the active cluster. All kinds keeps the overview, \
             problems and alerts live; Open views only watches a kind while a view shows it. A \
             cluster's own choice, made from its header or in Cluster overrides, overrides this.",
            draft.is_edited(&Field::WatchMode),
            theme,
        )
        .child(
            h_flex().gap_1().children(
                [
                    (WatchMode::All, "All kinds"),
                    (WatchMode::OnView, "Open views only"),
                ]
                .map(|(mode, label)| {
                    let stop = Stop::WatchMode(mode);
                    ring(
                        page,
                        stop,
                        theme,
                        Button::new(stop.id())
                            .small()
                            .label(label)
                            .selected(shown.watch_mode == mode)
                            .on_click(registry::on_click(EditWatchMode { mode })),
                    )
                }),
            ),
        )
        .into_any_element()),
        Section::Thresholds => settings_thresholds::render_global(page, shown, theme),
        Section::ClusterOverrides => {
            let rows = settings_thresholds::override_rows(shown, clusters, &page.thresholds.added);
            let candidates = settings_thresholds::add_candidates(clusters, &rows);
            settings_thresholds::render_overrides(page, shown, &rows, &candidates, theme)
        }
        Section::Logs => logs_paths::logs_body(page, theme),
        Section::Paths => logs_paths::paths_body(page, shown, theme),
        Section::Safety => one(logs_paths::safety_body(page, shown, theme)),
        Section::Network => network::body(page, &shown.proxy, invalid, theme),
    }
}

/// A titled block of controls, saying when it holds an unsaved edit.
pub(super) fn group(title: &'static str, text: &'static str, edited: bool, theme: &Theme) -> Div {
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

fn footer(page: &SettingsPage, invalid: bool, theme: &Theme) -> impl IntoElement {
    let draft = &page.draft;
    let dirty = draft.is_dirty();
    // A threshold still being typed is entered by Save and dropped by Revert.
    let typing = page.thresholds.entry.is_some();
    // Revert also drops what is typed in the proxy fields.
    let proxy_typed = page.network.is_typed();
    let saving = page.saving.is_some();
    let (note, color): (SharedString, Hsla) = match &page.save_note {
        _ if saving => ("Saving…".into(), theme.muted_foreground),
        Some(why) => (why.clone().into(), theme.danger),
        None if draft.has_rejected() => (REJECTED.into(), theme.danger),
        None if dirty => ("Unsaved changes".into(), theme.muted_foreground),
        None => ("No unsaved changes".into(), theme.muted_foreground),
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
                .min_w_0()
                .text_xs()
                .text_color(color)
                .aria_label(note.clone())
                .child(note)
                .test_support(),
        )
        .child(ring(
            page,
            Stop::Revert,
            theme,
            Button::new("settings-revert")
                .small()
                .label("Revert")
                .disabled(saving || !(dirty || typing || proxy_typed))
                .on_click(registry::on_click(RevertSettings)),
        ))
        .child(ring(
            page,
            Stop::Save,
            theme,
            Button::new("settings-save")
                .small()
                .primary()
                .label("Save")
                // With a value rejected, Save shows its field rather than writing.
                .disabled(
                    saving
                        || invalid
                        || !(can_save(draft, invalid) || typing || draft.has_rejected()),
                )
                .on_click(registry::on_click(SaveSettings)),
        ))
}
