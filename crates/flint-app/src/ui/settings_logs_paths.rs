//! The Logs, Paths and Write safety sections of the Settings view (#742): the three log buffer
//! limits, the extra kubeconfig files, the `PATH` and `helm` overrides, and
//! `protect_local_clusters`.
//!
//! The limits and the two overrides are text fields ([`TextField`]). What is typed is checked as
//! it is typed ([`TextField::parse`]): a valid value is an edit in the draft, an invalid one is
//! an error under its field, which keeps Save disabled until it is fixed or reverted. Each field
//! also has a registered action (`settings::EditLogBufferLines`, ...), so the palette and
//! automation set the same draft. The kubeconfig files are a list: Add… opens a file picker,
//! Remove drops one.
//!
//! Turning protection on is an ordinary edit. Turning it off is an edit too, but Save asks first
//! (`workspace_settings_local.rs`), and the section says so beside the toggle.

// module-tests: settings_logs_paths_tests.rs

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use flint_core::Settings;
use flint_core::settings::{
    LOG_BUFFER_LINES_RANGE, LOG_BUFFER_MIB_RANGE, LOG_TOTAL_MIB_RANGE, is_storable_path,
};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Escape, IndentInline, Input, InputState, OutdentInline};
use gpui_kit::component::{Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use super::registry;
use super::settings_draft::{Change, Field};
use super::settings_view::{
    NextSettingsControl, PreviousSettingsControl, Section, SettingsPage, Stop, group, ring,
};

/// Set the lines each log tab keeps, in the Settings draft.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditLogBufferLines {
    pub lines: u32,
}

/// Set the MiB each log tab keeps, in the Settings draft.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditLogBufferMib {
    pub mib: u32,
}

/// Set the MiB all log tabs keep together, in the Settings draft.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditLogTotalMib {
    pub mib: u32,
}

/// Set the `PATH` override in the Settings draft; empty uses the login shell's.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditPathOverride {
    pub path: String,
}

/// Set the `helm` binary in the Settings draft; empty finds it on `PATH`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditHelmPath {
    pub path: String,
}

/// Add a kubeconfig file to the list in the Settings draft.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct AddKubeconfigPath {
    pub path: String,
}

/// Drop a kubeconfig file from the list in the Settings draft.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct RemoveKubeconfigPath {
    pub path: String,
    /// The row clicked, so a file listed twice loses that row and not its first; ignored unless
    /// that row still holds `path`. The palette names the file alone.
    #[serde(default)]
    pub index: Option<usize>,
}

/// Turn protection of local clusters on or off in the Settings draft. Off is confirmed at Save.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditProtectLocalClusters {
    pub enabled: bool,
}

actions!(
    settings,
    [
        /// Choose kubeconfig files to add to the list in Settings.
        ChooseKubeconfigPaths
    ]
);

/// A text field of these sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextField {
    LogBufferLines,
    LogBufferMib,
    LogTotalMib,
    Path,
    Helm,
}

impl TextField {
    pub const ALL: [TextField; 5] = [
        TextField::LogBufferLines,
        TextField::LogBufferMib,
        TextField::LogTotalMib,
        TextField::Path,
        TextField::Helm,
    ];

    /// The section it is in.
    pub fn section(self) -> Section {
        match self {
            TextField::LogBufferLines | TextField::LogBufferMib | TextField::LogTotalMib => {
                Section::Logs
            }
            TextField::Path | TextField::Helm => Section::Paths,
        }
    }

    pub fn field(self) -> Field {
        match self {
            TextField::LogBufferLines => Field::LogBufferLines,
            TextField::LogBufferMib => Field::LogBufferMib,
            TextField::LogTotalMib => Field::LogTotalMib,
            TextField::Path => Field::Path,
            TextField::Helm => Field::Helm,
        }
    }

    /// The element id of its input.
    pub fn id(self) -> &'static str {
        match self {
            TextField::LogBufferLines => "settings-log-buffer-lines",
            TextField::LogBufferMib => "settings-log-buffer-mib",
            TextField::LogTotalMib => "settings-log-total-mib",
            TextField::Path => "settings-path",
            TextField::Helm => "settings-helm",
        }
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            TextField::LogBufferLines | TextField::LogBufferMib | TextField::LogTotalMib => "",
            TextField::Path => "The login shell's PATH",
            TextField::Helm => "helm on PATH",
        }
    }

    /// What it shows for `settings`.
    pub fn text(self, settings: &Settings) -> String {
        match self {
            TextField::LogBufferLines => settings.log_buffer_lines.to_string(),
            TextField::LogBufferMib => settings.log_buffer_mib.to_string(),
            TextField::LogTotalMib => settings.log_total_mib.to_string(),
            TextField::Path => settings.path.clone().unwrap_or_default(),
            TextField::Helm => settings
                .helm
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        }
    }

    /// The edit `text` makes, or why it is not valid. `home` expands a leading `~` to check that
    /// a file exists.
    pub fn parse(self, text: &str, home: Option<&Path>) -> Result<Change, String> {
        let text = text.trim();
        match self {
            TextField::LogBufferLines => {
                bounded(text, LOG_BUFFER_LINES_RANGE, "lines").map(Change::LogBufferLines)
            }
            TextField::LogBufferMib => {
                bounded(text, LOG_BUFFER_MIB_RANGE, "MiB").map(Change::LogBufferMib)
            }
            TextField::LogTotalMib => {
                bounded(text, LOG_TOTAL_MIB_RANGE, "MiB").map(Change::LogTotalMib)
            }
            TextField::Path => parse_path_override(text).map(Change::Path),
            TextField::Helm => parse_helm(text, home).map(Change::Helm),
        }
    }
}

/// A whole number within `range`, with `_`, `,` or spaces between digits allowed.
fn bounded(text: &str, range: std::ops::RangeInclusive<u32>, unit: &str) -> Result<u32, String> {
    let digits: String = text
        .chars()
        .filter(|c| !matches!(c, '_' | ',' | ' '))
        .collect();
    let out_of_range = || {
        format!(
            "From {} to {} {unit}.",
            grouped(*range.start()),
            grouped(*range.end())
        )
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("A whole number. {}", out_of_range()));
    }
    match digits.parse::<u32>() {
        Ok(n) if range.contains(&n) => Ok(n),
        _ => Err(out_of_range()),
    }
}

/// `n` with thousands separated by commas.
fn grouped(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The `PATH` override: empty is none; otherwise absolute folders separated by colons. An empty
/// or relative entry would make programs run from Flint's working folder.
fn parse_path_override(text: &str) -> Result<Option<String>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    match text.split(':').find(|e| !Path::new(e).is_absolute()) {
        Some("") => Err("An empty entry (two colons, or one at either end) is not allowed.".into()),
        Some(bad) => Err(format!(
            "\"{bad}\" is not an absolute folder. Separate absolute folders with colons."
        )),
        None => Ok(Some(text.to_owned())),
    }
}

/// The `helm` binary: empty is the one on `PATH`; otherwise an existing file, absolute or under
/// `~`.
fn parse_helm(text: &str, home: Option<&Path>) -> Result<Option<PathBuf>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(text);
    existing_file(&path, home)?;
    Ok(Some(path))
}

/// Whether `path` names a file Flint can store and read: absolute or under `~`, and there.
pub fn existing_file(path: &Path, home: Option<&Path>) -> Result<(), String> {
    if !is_storable_path(path) {
        return Err("Use an absolute path, or one that starts with ~/.".into());
    }
    let expanded = match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => home.join(rest),
        (Ok(_), None) => return Err("~ cannot be expanded: HOME is not set.".into()),
        _ => path.to_owned(),
    };
    match std::fs::metadata(&expanded) {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(_) => Err(format!("{} is not a file.", path.display())),
        Err(_) => Err(format!("There is no file at {}.", path.display())),
    }
}

/// The kubeconfig list with `path` added, or why it cannot be.
pub fn with_kubeconfig(
    paths: &[PathBuf],
    path: &Path,
    home: Option<&Path>,
) -> Result<Vec<PathBuf>, String> {
    existing_file(path, home)?;
    // The same file, however it is written: `~/…` or the full path, or through a link.
    let file = |p: &Path| {
        let expanded = match (p.strip_prefix("~"), home) {
            (Ok(rest), Some(home)) => home.join(rest),
            _ => p.to_owned(),
        };
        std::fs::canonicalize(&expanded).unwrap_or(expanded)
    };
    let adding = file(path);
    if paths.iter().any(|p| p == path || file(p) == adding) {
        return Err(format!("{} is already in the list.", path.display()));
    }
    let mut paths = paths.to_vec();
    paths.push(path.to_owned());
    Ok(paths)
}

/// What these sections hold beside the draft: their inputs, once Settings has been opened in a
/// window, and what is wrong with each field.
#[derive(Default)]
pub struct LogsPathsFields {
    pub inputs: Option<Inputs>,
    /// Why a field's text is not valid. While any is set, Save is disabled.
    pub errors: BTreeMap<TextField, String>,
    /// Why the last kubeconfig file could not be added.
    pub kubeconfig_note: Option<String>,
    /// Put what the view shows into every field once, the focused one too (Revert, Reload).
    pub resync: bool,
    /// Fields typed in while a Save was being written, read again once it is answered.
    pub typed_while_saving: BTreeSet<TextField>,
}

impl LogsPathsFields {
    /// The input of `field`, once made.
    pub fn input(&self, field: TextField) -> Option<&Entity<InputState>> {
        self.inputs
            .as_ref()
            .and_then(|i| i.fields.iter().find(|(f, _)| *f == field))
            .map(|(_, input)| input)
    }

    /// The field whose input has focus.
    pub fn focused(&self, window: &Window, cx: &App) -> Option<TextField> {
        let inputs = self.inputs.as_ref()?;
        inputs
            .fields
            .iter()
            .find(|(_, input)| input.read(cx).focus_handle(cx).is_focused(window))
            .map(|(field, _)| *field)
    }
}

/// The text inputs, with what they report.
pub struct Inputs {
    pub fields: Vec<(TextField, Entity<InputState>)>,
    pub _subscriptions: Vec<Subscription>,
}

/// The element id of the Remove button of the `index`th kubeconfig file. Ids are kept for the
/// life of the app, one per position ever shown, so a long list costs a few bytes once.
pub fn remove_id(index: usize) -> &'static str {
    static IDS: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
    let mut ids = IDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    while ids.len() <= index {
        let id = format!("settings-kubeconfig-remove-{}", ids.len());
        ids.push(Box::leak(id.into_boxed_str()));
    }
    ids[index]
}

/// The stops of the Logs section, in Tab order.
pub fn logs_stops() -> Vec<Stop> {
    vec![
        Stop::Text(TextField::LogBufferLines),
        Stop::Text(TextField::LogBufferMib),
        Stop::Text(TextField::LogTotalMib),
    ]
}

/// The stops of the Paths section with `kubeconfigs` files listed, in Tab order.
pub fn paths_stops(kubeconfigs: usize) -> Vec<Stop> {
    let mut stops: Vec<Stop> = (0..kubeconfigs).map(Stop::RemoveKubeconfig).collect();
    stops.extend([
        Stop::AddKubeconfig,
        Stop::Text(TextField::Path),
        Stop::Text(TextField::Helm),
    ]);
    stops
}

/// The ring and Tab handling around a text field: inside the field, Tab and Shift-Tab go on
/// round the view and Escape leaves the field for the view, as they do outside it.
fn text_input(
    page: &SettingsPage,
    field: TextField,
    theme: &Theme,
) -> Option<impl IntoElement + use<>> {
    let input = page.logs_paths.input(field)?;
    let error = page.logs_paths.errors.get(&field).cloned();
    let on = page.cursor == Some(Stop::Text(field));
    let border = if error.is_some() {
        theme.danger
    } else if on {
        theme.ring
    } else {
        theme.background
    };
    let view = page.focus.clone();
    Some(
        v_flex()
            .gap_1()
            .child(
                div()
                    .w(px(320.))
                    .rounded_md()
                    .border_2()
                    .border_color(border)
                    // The field's own Tab, Shift-Tab and Escape, caught before it handles
                    // them: Tab goes on round the view, Escape leaves the field for the view.
                    .capture_action({
                        let view = view.clone();
                        move |_: &IndentInline, window, cx| {
                            view.focus(window, cx);
                            window.dispatch_action(NextSettingsControl.boxed_clone(), cx);
                            cx.stop_propagation();
                        }
                    })
                    .capture_action({
                        let view = view.clone();
                        move |_: &OutdentInline, window, cx| {
                            view.focus(window, cx);
                            window.dispatch_action(PreviousSettingsControl.boxed_clone(), cx);
                            cx.stop_propagation();
                        }
                    })
                    .capture_action(move |_: &Escape, window, cx| {
                        view.focus(window, cx);
                        cx.stop_propagation();
                    })
                    .child(Input::new(input).id(field.id()).small()),
            )
            .when_some(error, |el, error| {
                el.child(
                    div()
                        .id(error_id(field))
                        .text_xs()
                        .text_color(theme.danger)
                        .aria_label(error.clone())
                        .child(error)
                        .test_support(),
                )
            }),
    )
}

fn error_id(field: TextField) -> &'static str {
    match field {
        TextField::LogBufferLines => "settings-log-buffer-lines-error",
        TextField::LogBufferMib => "settings-log-buffer-mib-error",
        TextField::LogTotalMib => "settings-log-total-mib-error",
        TextField::Path => "settings-path-error",
        TextField::Helm => "settings-helm-error",
    }
}

/// The Logs section's rows, in [`content_row`] order.
pub fn logs_body(page: &SettingsPage, theme: &Theme) -> Vec<AnyElement> {
    let draft = &page.draft;
    let block = |field: TextField, title, text| {
        group(title, text, draft.is_edited(field.field()), theme)
            .children(text_input(page, field, theme))
    };
    vec![
        block(
            TextField::LogBufferLines,
            "Lines per log tab",
            "How many lines each log tab keeps before dropping its oldest, from 1,000 to \
             1,000,000. Applies to log tabs opened after Save.",
        )
        .into_any_element(),
        block(
            TextField::LogBufferMib,
            "Memory per log tab (MiB)",
            "The most memory each log tab's lines may take, from 8 to 1,024 MiB; whichever \
             limit comes first applies. Applies to log tabs opened after Save.",
        )
        .mt_4()
        .into_any_element(),
        block(
            TextField::LogTotalMib,
            "Memory for all log tabs (MiB)",
            "The most memory all log tabs may take together, from 64 to 16,384 MiB. Past it, \
             background tabs drop their oldest lines first. Never below one tab's own limit. \
             Applies at Save.",
        )
        .mt_4()
        .into_any_element(),
    ]
}

/// The Paths section's rows, in [`content_row`] order: each kubeconfig file is a row of its
/// own, so the keyboard cursor can bring any of them into view.
pub fn paths_body(page: &SettingsPage, shown: &Settings, theme: &Theme) -> Vec<AnyElement> {
    let draft = &page.draft;
    let paths = &shown.kubeconfig_paths;
    let mut rows = vec![
        group(
            "Extra kubeconfig files",
            "Loaded after KUBECONFIG or ~/.kube/config, in this order. Clusters reload after \
             Save, as after editing settings.toml by hand.",
            draft.is_edited(Field::KubeconfigPaths),
            theme,
        )
        .into_any_element(),
    ];
    if paths.is_empty() {
        rows.push(
            div()
                .mt_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("None")
                .into_any_element(),
        );
    }
    rows.extend(paths.iter().enumerate().map(|(index, path)| {
        let shown_path = path.display().to_string();
        h_flex()
            .id(("settings-kubeconfig-path", index))
            .max_w(px(560.))
            .when(index == 0, |el| el.mt_2())
            .when(index > 0, |el| el.mt_1())
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .truncate()
                    .child(shown_path.clone()),
            )
            .child(ring(
                page,
                Stop::RemoveKubeconfig(index),
                theme,
                Button::new(remove_id(index))
                    .xsmall()
                    .label("Remove")
                    .tooltip(format!("Remove {shown_path} from the list"))
                    .on_click(registry::on_click(RemoveKubeconfigPath {
                        path: shown_path.clone(),
                        index: Some(index),
                    })),
            ))
            .into_any_element()
    }));
    rows.push(
        v_flex()
            .mt_1()
            .gap_1()
            .child(
                h_flex().child(ring(
                    page,
                    Stop::AddKubeconfig,
                    theme,
                    Button::new(Stop::AddKubeconfig.id())
                        .xsmall()
                        .label("Add…")
                        .on_click(registry::on_click(ChooseKubeconfigPaths)),
                )),
            )
            .when_some(page.logs_paths.kubeconfig_note.clone(), |el, note| {
                el.child(
                    div()
                        .id("settings-kubeconfig-note")
                        .text_xs()
                        .text_color(theme.danger)
                        .aria_label(note.clone())
                        .child(note)
                        .test_support(),
                )
            })
            .into_any_element(),
    );
    rows.push(
        group(
            "PATH",
            "Where Flint finds kubectl plugins, helm and credential helpers: absolute folders \
             separated by colons. Empty uses the login shell's PATH. Applies after a restart.",
            draft.is_edited(Field::Path),
            theme,
        )
        .mt_4()
        .children(text_input(page, TextField::Path, theme))
        .into_any_element(),
    );
    rows.push(
        group(
            "helm",
            "The helm binary for Helm releases. Empty finds helm on PATH.",
            draft.is_edited(Field::Helm),
            theme,
        )
        .mt_4()
        .children(text_input(page, TextField::Helm, theme))
        .into_any_element(),
    );
    rows
}

/// The row of the section's content that holds `stop`, from the top, for a Paths list of
/// `kubeconfigs` files: what [`logs_body`] and [`paths_body`] lay out, and one row for each
/// other section. `None` for a stop outside the content (the section list, the footer, the
/// banner).
pub fn content_row(stop: Stop, kubeconfigs: usize) -> Option<usize> {
    // The list's heading, then its files (or "None"), then Add….
    let add = 1 + kubeconfigs.max(1);
    match stop {
        Stop::Text(TextField::LogBufferLines) => Some(0),
        Stop::Text(TextField::LogBufferMib) => Some(1),
        Stop::Text(TextField::LogTotalMib) => Some(2),
        Stop::RemoveKubeconfig(index) => Some(1 + index),
        Stop::AddKubeconfig => Some(add),
        Stop::Text(TextField::Path) => Some(add + 1),
        Stop::Text(TextField::Helm) => Some(add + 2),
        Stop::Appearance(_)
        | Stop::DesktopNotifications
        | Stop::WatchMode(_)
        | Stop::ProtectLocalClusters => Some(0),
        Stop::Done
        | Stop::Reload
        | Stop::Revert
        | Stop::SaveAnyway
        | Stop::Save
        | Stop::Section(_) => None,
    }
}

/// The confirmation's title.
pub const UNPROTECT_TITLE: &str = "Turn off protection for local clusters?";

/// What the confirmation says turning protection off exposes.
pub const UNPROTECT_TEXT: &str = "Writes to local clusters (kind, minikube, Docker Desktop and \
    OrbStack contexts with an API server on this machine) will no longer be gated: Flint will \
    change them without asking for an unlock. Other clusters stay protected. Cancel leaves all \
    your edits unsaved.";

/// Added when `settings.toml` changed outside Settings under the draft.
pub const UNPROTECT_ON_DISK: &str = " settings.toml was also changed outside Settings since you \
    started editing; only the settings you edited are written.";

/// The footer when the draft or the file changed while the confirmation was open.
pub const UNPROTECT_STALE: &str = "Not saved: the settings changed while you were \
    confirming. Review them and save again.";

/// What the Write safety section says about turning protection off.
pub const UNPROTECT_NOTE: &str = "Save will ask before it turns protection off: writes to local clusters will no longer be \
     gated.";

/// The Write safety section.
pub fn safety_body(page: &SettingsPage, shown: &Settings, theme: &Theme) -> AnyElement {
    let draft = &page.draft;
    let on = shown.protect_local_clusters;
    group(
        "Protect local clusters",
        "Local clusters (kind, minikube, Docker Desktop and OrbStack contexts with an API server \
         on this machine) start unlocked, so Flint changes them without asking for an unlock. \
         Turn this on to gate writes to them like any other cluster. Other clusters stay \
         protected either way.",
        draft.is_edited(Field::ProtectLocalClusters),
        theme,
    )
    .child(
        h_flex().child(ring(
            page,
            Stop::ProtectLocalClusters,
            theme,
            Checkbox::new(Stop::ProtectLocalClusters.id())
                .label("Protect local clusters too")
                .checked(on)
                .on_click(registry::on_click(EditProtectLocalClusters {
                    enabled: !on,
                })),
        )),
    )
    .when(draft.loosens_protection(), |el| {
        el.child(
            div()
                .id("settings-unprotect-note")
                .text_xs()
                .text_color(theme.warning)
                .aria_label(UNPROTECT_NOTE)
                .child(UNPROTECT_NOTE)
                .test_support(),
        )
    })
    .into_any_element()
}
