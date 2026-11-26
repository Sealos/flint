//! The Network section of the Settings view (#743): the proxy for cluster connections
//! (`[proxy]` in `settings.toml`), applied at once without a restart.
//!
//! Unlike the other sections, the proxy is not part of the Settings draft: it has its own Apply
//! button, because applying it reconnects every cluster ([`flint_core::CoreHandle::apply_proxy`]).
//! What is typed is checked as it is typed ([`proposed`], through
//! [`flint_core::settings::ProxySetting::check`]); Apply then asks first, saying what reconnects
//! and what ends (`workspace_settings_network.rs`), and the section shows how each cluster came
//! back.
//!
//! A proxy URL can hold a password. It is never shown: the saved URL is shown with its
//! credentials masked ([`url_text`]), and the field shows that masked text until it is changed.
//! Left as it is, the saved URL, password included, is kept. Errors come from
//! [`flint_core::settings::ProxySetting::check`] and the core, which never quote credentials.
//!
//! The section is self-contained: a slice that moves Settings elsewhere moves this module, its
//! workspace side, and the [`Stop::Network`] arms.

// module-tests: settings_network_tests.rs

use flint_core::ClusterId;
use flint_core::handle::ProxyReconnectReport;
use flint_core::settings::ProxySetting;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, IndentInline, Input, InputState, OutdentInline};
use gpui_kit::component::{Disableable as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::Deserialize;

use super::dock_model::LiveSessions;
use super::registry;
use super::settings_view::{
    NextSettingsControl, PreviousSettingsControl, SettingsPage, Stop, group, ring,
};

/// Set the proxy URL field in Settings' Network section. Applied only by `ApplyProxy`.
#[derive(Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditProxyUrl {
    pub url: String,
}

/// The URL can hold a password: never printed.
impl std::fmt::Debug for EditProxyUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditProxyUrl").finish_non_exhaustive()
    }
}

/// Set the no-proxy field in Settings' Network section: entries separated by commas.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = settings, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditNoProxy {
    pub no_proxy: String,
}

actions!(
    settings,
    [
        /// Apply the proxy typed in Settings' Network section: asks first, then saves it and
        /// reconnects every cluster on it.
        ApplyProxy
    ]
);

/// What stands in for a proxy URL's user name and password.
pub const MASK: &str = "•••";

/// A control of the Network section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NetworkStop {
    Url,
    NoProxy,
    Apply,
}

impl NetworkStop {
    pub const FIELDS: [NetworkStop; 2] = [NetworkStop::Url, NetworkStop::NoProxy];

    pub fn id(self) -> &'static str {
        match self {
            NetworkStop::Url => "settings-proxy-url",
            NetworkStop::NoProxy => "settings-no-proxy",
            NetworkStop::Apply => "settings-proxy-apply",
        }
    }

    /// The row of the section's content that holds it ([`body`]).
    pub fn row(self) -> usize {
        match self {
            NetworkStop::Url => 0,
            NetworkStop::NoProxy => 1,
            NetworkStop::Apply => 2,
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            NetworkStop::Url => "None: HTTPS_PROXY from the environment, if set",
            NetworkStop::NoProxy => "localhost, .internal.example.com, 10.0.0.0/8",
            NetworkStop::Apply => "",
        }
    }
}

/// The stops of the Network section, in Tab order.
pub fn stops() -> Vec<Stop> {
    [NetworkStop::Url, NetworkStop::NoProxy, NetworkStop::Apply]
        .map(Stop::Network)
        .to_vec()
}

/// Whether `url` may hold a user name or password. Any `@` counts, so a URL that cannot be read
/// is never shown as it is.
pub fn has_credentials(url: &str) -> bool {
    url.contains('@')
}

/// What the URL field shows for `saved`: the URL itself, or, when it holds credentials, its
/// address with [`MASK`] in their place. Never a password.
pub fn url_text(saved: &ProxySetting) -> String {
    let Some(url) = saved
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    else {
        return String::new();
    };
    if !has_credentials(url) {
        return url.to_owned();
    }
    let shown = saved.shown_url().unwrap_or_default();
    match shown.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{MASK}@{rest}"),
        None => shown,
    }
}

/// What the no-proxy field shows for `saved`.
pub fn no_proxy_text(saved: &ProxySetting) -> String {
    saved.no_proxy.join(", ")
}

/// The setting `url` and `no_proxy`, as typed, would apply over `saved`, or why it cannot. A URL
/// field that still shows the saved one ([`url_text`]) keeps it, password included.
pub fn proposed(saved: &ProxySetting, url: &str, no_proxy: &str) -> Result<ProxySetting, String> {
    let url = url.trim();
    let url = if url == url_text(saved) {
        saved.url.clone()
    } else if url.contains(MASK) || url.contains('•') {
        return Err(
            "The saved password is hidden: type the whole proxy URL again, password included."
                .to_owned(),
        );
    } else {
        (!url.is_empty()).then(|| url.to_owned())
    };
    let setting = ProxySetting {
        url,
        no_proxy: no_proxy
            .split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .collect(),
    };
    setting.check().map_err(|why| capitalized(&why))?;
    Ok(setting)
}

/// Whether `a` and `b` are the same setting, as `settings.toml` would save them.
pub fn same(a: &ProxySetting, b: &ProxySetting) -> bool {
    let normal = |s: &ProxySetting| {
        (
            s.url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_owned),
            s.no_proxy
                .iter()
                .map(|e| e.trim())
                .filter(|e| !e.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )
    };
    normal(a) == normal(b)
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => {
            let mut out: String = first.to_uppercase().collect();
            out.push_str(chars.as_str());
            if !out.ends_with('.') {
                out.push('.');
            }
            out
        }
        None => String::new(),
    }
}

/// The confirmation's title.
pub const APPLY_TITLE: &str = "Apply the new proxy?";

/// What the confirmation says, with `forwards` port-forwards running and `live` sessions open.
pub fn impact_text(setting: &ProxySetting, forwards: usize, live: LiveSessions) -> String {
    let route = match setting.shown_url() {
        Some(url) => format!("By default they go through {url}"),
        None => "By default they go without a proxy of Flint's own (HTTPS_PROXY from the \
                 environment, if set)"
            .into(),
    };
    let entries: Vec<&str> = setting
        .no_proxy
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .collect();
    let except = if entries.is_empty() {
        String::new()
    } else {
        format!(", except servers matching {}", entries.join(", "))
    };
    let mut text = format!(
        "Every cluster connects again: the selected cluster, the running set's members and the \
         clusters watched in the background. {route}{except}; a cluster with its own proxy-url \
         in kubeconfig keeps it. Until each has reconnected, nothing new connects on it, and \
         writes waiting for review are refused: review them again once it is back."
    );
    text.push_str(&match forwards {
        0 => String::new(),
        1 => " 1 port-forward keeps its local port and reconnects once its cluster is back; if \
              it can't, it ends and says why."
            .into(),
        n => format!(
            " {n} port-forwards keep their local ports and reconnect once their clusters are \
             back; one that can't ends and says why."
        ),
    });
    if !live.is_empty() {
        text.push_str(&format!(
            " {} open: they end on a cluster the new setting reaches another way.",
            live.described()
        ));
    }
    text.push_str(
        " A cluster reached another way must also be unlocked, trusted and kept watching again.",
    );
    text
}

/// Where an Apply is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Sent to the core, not answered yet.
    Applying,
    /// Saved and in use; these clusters and this many port-forwards are connecting again.
    Reconnecting {
        waiting: Vec<ClusterId>,
        forwards: usize,
    },
    /// How each came back.
    Done(ProxyReconnectReport),
    /// Refused: nothing changed.
    Failed(String),
}

impl Progress {
    /// The lines the section shows for it.
    pub fn lines(&self) -> Vec<String> {
        let names = |ids: &[ClusterId]| ids.join(", ");
        let waiting_forwards = |n: usize| match n {
            0 => None,
            1 => Some("Reconnecting 1 port-forward on its local port…".to_owned()),
            n => Some(format!(
                "Reconnecting {n} port-forwards on their local ports…"
            )),
        };
        match self {
            Progress::Applying => vec!["Applying the proxy…".into()],
            Progress::Failed(why) => vec![format!("Not applied: {why}")],
            Progress::Reconnecting { waiting, forwards } => {
                let mut lines = vec![if waiting.is_empty() {
                    "Applied. No cluster was connected, so none reconnects.".to_owned()
                } else {
                    format!("Applied. Reconnecting: {}…", names(waiting))
                }];
                lines.extend(waiting_forwards(*forwards));
                lines
            }
            Progress::Done(report) => {
                let mut lines = vec![];
                if report.reconnected.is_empty() && report.failed.is_empty() {
                    lines.push("Applied. No cluster was connected, so none reconnected.".into());
                }
                if !report.reconnected.is_empty() {
                    lines.push(format!("Reconnected: {}.", names(&report.reconnected)));
                }
                for (id, why) in &report.failed {
                    lines.push(format!("Not reconnected: {id}: {why}."));
                }
                if !report.rerouted.is_empty() {
                    lines.push(format!(
                        "Now reached another way: {}. Unlock, trust and keep watching them again \
                         if they were.",
                        names(&report.rerouted)
                    ));
                }
                for forward in &report.forwards {
                    let port = forward
                        .local_port
                        .map_or(String::new(), |port| format!(" on local port {port}"));
                    let what = format!("{} of {}{port}", forward.target, forward.cluster);
                    lines.push(match &forward.ended {
                        None => format!("Port-forward reconnected: {what}."),
                        Some(why) => format!("Port-forward ended: {what}: {why}."),
                    });
                }
                lines
            }
        }
    }

    pub fn failed(&self) -> bool {
        matches!(self, Progress::Failed(_))
            || matches!(self, Progress::Done(report)
                if !report.failed.is_empty() || report.forwards.iter().any(|f| f.ended.is_some()))
    }
}

/// What the Network section holds beside the file: its inputs, once Settings has been opened in
/// a window, what they propose, and the last Apply.
#[derive(Default)]
pub struct NetworkFields {
    pub inputs: Option<NetworkInputs>,
    /// The setting the fields propose, or why they cannot; `None` until checked.
    pub checked: Option<Result<ProxySetting, String>>,
    /// The fields typed in since they last showed the file; the others follow it.
    pub typed: std::collections::BTreeSet<NetworkStop>,
    /// A field was typed in while an Apply was answered: what it holds is kept.
    pub typed_while_applying: bool,
    /// Whether the URL field was last masked for the credentials it holds.
    pub url_masked: Option<bool>,
    /// Put the file's setting into the fields once, as the workspace next draws.
    pub resync: bool,
    pub progress: Option<Progress>,
    /// The setting applied, until the file holds it: then the fields show it, masked.
    pub applied: Option<ProxySetting>,
    /// The Apply under way, or the wait for its report.
    pub task: Option<Task<()>>,
}

impl NetworkFields {
    pub fn input(&self, stop: NetworkStop) -> Option<&Entity<InputState>> {
        self.inputs
            .as_ref()
            .and_then(|i| i.fields.iter().find(|(s, _)| *s == stop))
            .map(|(_, input)| input)
    }

    /// The field whose input has focus.
    pub fn focused(&self, window: &Window, cx: &App) -> Option<NetworkStop> {
        let inputs = self.inputs.as_ref()?;
        inputs
            .fields
            .iter()
            .find(|(_, input)| input.read(cx).focus_handle(cx).is_focused(window))
            .map(|(stop, _)| *stop)
    }

    /// Whether a field was typed in since it last showed the file.
    pub fn is_typed(&self) -> bool {
        !self.typed.is_empty()
    }

    /// Whether an Apply waits for the core's answer.
    pub fn applying(&self) -> bool {
        self.progress == Some(Progress::Applying)
    }

    /// The setting Apply would apply over `saved`: valid, and not the one saved.
    pub fn to_apply(&self, saved: &ProxySetting) -> Option<&ProxySetting> {
        match &self.checked {
            Some(Ok(setting)) if !same(setting, saved) && !self.applying() => Some(setting),
            _ => None,
        }
    }

    /// Why the fields cannot be applied, if they cannot.
    pub fn error(&self) -> Option<&str> {
        match &self.checked {
            Some(Err(why)) => Some(why),
            _ => None,
        }
    }
}

/// The two text inputs, with what they report.
pub struct NetworkInputs {
    pub fields: Vec<(NetworkStop, Entity<InputState>)>,
    pub _subscriptions: Vec<Subscription>,
}

/// Makes the input for `stop` showing `text`.
pub fn new_input(
    stop: NetworkStop,
    text: String,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(stop.placeholder())
            .default_value(text)
    })
}

/// The Network section's rows, in [`NetworkStop::row`] order, over the file's `saved` setting.
pub fn body(
    page: &SettingsPage,
    saved: &ProxySetting,
    invalid: bool,
    theme: &Theme,
) -> Vec<AnyElement> {
    let network = &page.network;
    let error = network.error().map(str::to_owned);
    vec![
        group(
            "Proxy",
            "The proxy for cluster connections: http://, socks5:// or socks5h://host:port, with \
             an optional user:password@. Empty uses HTTPS_PROXY from the environment, if set. A \
             cluster's own proxy-url in kubeconfig still wins. Applies at once with Apply, not \
             with Save.",
            network.typed.contains(&NetworkStop::Url),
            theme,
        )
        .children(field(page, NetworkStop::Url, error.clone(), theme))
        .into_any_element(),
        group(
            "No proxy",
            "Servers reached without the proxy, separated by commas: host names, domains \
             (.example.com), IP addresses and CIDR ranges, or * for all.",
            network.typed.contains(&NetworkStop::NoProxy),
            theme,
        )
        .children(field(page, NetworkStop::NoProxy, None, theme))
        .into_any_element(),
        apply_row(page, saved, invalid, theme).into_any_element(),
    ]
}

/// A text field with the ring and Tab handling of the view: inside it, Tab and Shift-Tab go on
/// round the view and Escape leaves it for the view.
fn field(
    page: &SettingsPage,
    stop: NetworkStop,
    error: Option<String>,
    theme: &Theme,
) -> Option<impl IntoElement + use<>> {
    let input = page.network.input(stop)?;
    let on = page.cursor == Some(Stop::Network(stop));
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
                    .w(px(420.))
                    .rounded_md()
                    .border_2()
                    .border_color(border)
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
                    .child(
                        Input::new(input)
                            .id(stop.id())
                            .small()
                            // A URL holding a password is masked; the eye shows it on request.
                            .when(stop == NetworkStop::Url, |input| input.mask_toggle()),
                    ),
            )
            .when_some(error, |el, error| {
                el.child(
                    div()
                        .id("settings-proxy-error")
                        .text_xs()
                        .text_color(theme.danger)
                        .aria_label(error.clone())
                        .child(error)
                        .test_support(),
                )
            }),
    )
}

/// The Apply button, what is in use, and the last Apply's report.
fn apply_row(
    page: &SettingsPage,
    saved: &ProxySetting,
    invalid: bool,
    theme: &Theme,
) -> impl IntoElement {
    let network = &page.network;
    let in_file = match saved.shown_url() {
        Some(url) if saved.url.as_deref().is_some_and(has_credentials) => {
            format!("Saved: {url} (credentials hidden)")
        }
        Some(url) => format!("Saved: {url}"),
        None => "Saved: none".to_owned(),
    };
    let report = network.progress.as_ref().map(|p| (p.lines(), p.failed()));
    v_flex()
        .gap_2()
        .max_w(px(560.))
        .child(
            h_flex()
                .gap_2()
                .child(ring(
                    page,
                    Stop::Network(NetworkStop::Apply),
                    theme,
                    Button::new(NetworkStop::Apply.id())
                        .small()
                        .primary()
                        .label("Apply…")
                        .tooltip("Save this proxy and reconnect every cluster on it")
                        .disabled(invalid || network.to_apply(saved).is_none())
                        .on_click(registry::on_click(ApplyProxy)),
                ))
                .child(
                    div()
                        .id("settings-proxy-saved")
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .aria_label(in_file.clone())
                        .child(in_file)
                        .test_support(),
                ),
        )
        .when_some(report, |el, (lines, failed)| {
            let text = lines.join("\n");
            el.child(
                v_flex()
                    .id("settings-proxy-report")
                    .gap_1()
                    .text_xs()
                    .text_color(if failed {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .aria_label(text)
                    .children(lines.into_iter().map(|line| div().child(line)))
                    .test_support(),
            )
        })
}

/// The section as automation reads it, over the file's `saved` setting.
#[cfg(any(test, feature = "automation"))]
pub fn view_state(
    network: &NetworkFields,
    saved: &ProxySetting,
    invalid: bool,
) -> super::view_state::NetworkState {
    let (reconnected, failed) = match &network.progress {
        Some(Progress::Done(report)) => (Some(report.reconnected.clone()), report.failed.clone()),
        _ => (None, Vec::new()),
    };
    super::view_state::NetworkState {
        saved_url: url_text(saved),
        saved_no_proxy: saved.no_proxy.clone(),
        typed: network.is_typed(),
        error: network.error().map(str::to_owned),
        can_apply: !invalid && network.to_apply(saved).is_some(),
        applying: network.applying(),
        report: network
            .progress
            .as_ref()
            .map(Progress::lines)
            .unwrap_or_default(),
        reconnected,
        failed,
    }
}
