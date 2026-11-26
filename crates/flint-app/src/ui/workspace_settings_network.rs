//! The workspace's side of the Network section of Settings ([`super::super::settings_network`]):
//! its actions and guards, the proxy fields, and Apply, which asks first and then reconnects
//! every cluster on the new proxy through the core ([`flint_core::CoreHandle::apply_proxy`]).
//!
//! Apply never happens unasked: the confirmation says every cluster reconnects, what ends, and
//! that writes waiting for review are refused until their cluster is back. Enter and Escape
//! cancel it; only its button applies. It holds for the setting and the saved proxy it was asked
//! over: if either changed before it was answered, nothing is applied and the section says so.

// module-tests: settings_network_tests.rs

use flint_core::ProxyApplied;
use flint_core::settings::ProxySetting;
use gpui_kit::component::input::InputEvent;
use gpui_kit::*;

use super::super::confirm::{self, DialogText};
use super::super::registry::{Bind, Guards};
use super::super::settings_network::{
    APPLY_TITLE, ApplyProxy, EditNoProxy, EditProxyUrl, NetworkInputs, NetworkStop, Progress,
    has_credentials, impact_text, new_input, no_proxy_text, proposed, same, url_text,
};
use super::super::settings_view::Stop;
use super::Workspace;

/// What the section says when the fields or the saved proxy changed during the confirmation.
pub const APPLY_STALE: &str =
    "the proxy changed while you were confirming. Review it and apply again.";

impl Workspace {
    pub(super) fn network_guards(guards: Guards<Self>) -> Guards<Self> {
        fn editable(ws: &Workspace, _: &App) -> bool {
            ws.settings_page.open && !ws.settings_page.network.applying()
        }
        guards
            .when::<EditProxyUrl>(editable)
            .when::<EditNoProxy>(editable)
            .when::<ApplyProxy>(|ws, cx| ws.settings_page.open && ws.proxy_to_apply(cx).is_some())
    }

    pub(super) fn bind_network_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<EditProxyUrl>(|ws, action, window, cx| {
            ws.set_network_text(NetworkStop::Url, action.url.clone(), window, cx)
        })
        .bind::<EditNoProxy>(|ws, action, window, cx| {
            ws.set_network_text(NetworkStop::NoProxy, action.no_proxy.clone(), window, cx)
        })
        .bind::<ApplyProxy>(|ws, _, window, cx| ws.ask_apply_proxy(window, cx))
    }

    /// The proxy `settings.toml` holds, as the core last reported it.
    fn saved_proxy(&self, cx: &App) -> ProxySetting {
        self.model.read(cx).settings.proxy.clone()
    }

    /// The setting Apply would apply: valid, not the saved one, and nothing in the way.
    fn proxy_to_apply(&self, cx: &App) -> Option<ProxySetting> {
        let model = self.model.read(cx);
        if model.settings_error.is_some() {
            return None;
        }
        self.settings_page
            .network
            .to_apply(&model.settings.proxy)
            .cloned()
    }

    /// Makes the proxy fields, the first time Settings opens in this window.
    pub(super) fn ensure_network_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_page.network.inputs.is_some() {
            return;
        }
        let saved = self.saved_proxy(cx);
        let mut fields = Vec::new();
        let mut subscriptions = Vec::new();
        for stop in NetworkStop::FIELDS {
            let text = match stop {
                NetworkStop::Url => url_text(&saved),
                _ => no_proxy_text(&saved),
            };
            let input = new_input(stop, text, window, cx);
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |ws: &mut Workspace, _, event, window, cx| match event {
                    InputEvent::Change => {
                        let network = &mut ws.settings_page.network;
                        network.typed.insert(stop);
                        network.typed_while_applying |= network.applying();
                        ws.recheck_proxy(cx);
                    }
                    InputEvent::Focus => {
                        if ws.settings_page.open {
                            ws.settings_page.cursor = Some(Stop::Network(stop));
                            cx.notify();
                        }
                    }
                    InputEvent::PressEnter { .. } => {
                        window.focus(&ws.settings_page.focus, cx);
                    }
                    _ => {}
                },
            ));
            fields.push((stop, input));
        }
        self.settings_page.network.inputs = Some(NetworkInputs {
            fields,
            _subscriptions: subscriptions,
        });
        self.recheck_proxy(cx);
    }

    /// The text in the proxy field `stop`, once Settings has made it.
    #[cfg(test)]
    pub(in crate::ui) fn network_field_text(&self, stop: NetworkStop, cx: &App) -> String {
        self.network_text(stop, cx)
    }

    /// Whether the URL field masks its text.
    #[cfg(test)]
    pub(in crate::ui) fn network_url_masked(&self, _: &App) -> bool {
        self.settings_page.network.url_masked == Some(true)
    }

    /// Where the last Apply is.
    #[cfg(test)]
    pub(in crate::ui) fn network_progress(&self) -> Option<Progress> {
        self.settings_page.network.progress.clone()
    }

    /// The text in `stop`'s input.
    fn network_text(&self, stop: NetworkStop, cx: &App) -> String {
        self.settings_page
            .network
            .input(stop)
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
    }

    /// Checks what the fields propose over the saved proxy.
    fn recheck_proxy(&mut self, cx: &mut Context<Self>) {
        let saved = self.saved_proxy(cx);
        let url = self.network_text(NetworkStop::Url, cx);
        let no_proxy = self.network_text(NetworkStop::NoProxy, cx);
        let checked = proposed(&saved, &url, &no_proxy);
        let network = &mut self.settings_page.network;
        // Typed back to what is saved: the fields follow the file again.
        if checked.as_ref().is_ok_and(|setting| same(setting, &saved)) {
            network.typed.clear();
        }
        if network.checked.as_ref() != Some(&checked) {
            network.checked = Some(checked);
            cx.notify();
        }
    }

    /// Puts the saved proxy into the fields while they were not typed in (the focused one aside,
    /// unless a resync asks for it), then checks them again. Called as the workspace draws.
    pub(super) fn sync_network_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_page.open || self.settings_page.network.inputs.is_none() {
            return;
        }
        let saved = self.saved_proxy(cx);
        let network = &mut self.settings_page.network;
        // Once the file holds what was applied, the fields show it, masked, focused or not.
        if network
            .applied
            .as_ref()
            .is_some_and(|applied| same(applied, &saved))
        {
            network.applied = None;
            // What was typed while it was being applied stays.
            if !std::mem::take(&mut network.typed_while_applying) {
                network.typed.clear();
                network.resync = true;
            }
        }
        let resync = std::mem::take(&mut network.resync);
        let focused = if resync {
            None
        } else {
            network.focused(window, cx)
        };
        let typed = network.typed.clone();
        let stale: Vec<_> = network
            .inputs
            .iter()
            .flat_map(|inputs| inputs.fields.iter())
            .filter(|(stop, _)| Some(*stop) != focused && !typed.contains(stop))
            .filter_map(|(stop, input)| {
                let want = match stop {
                    NetworkStop::Url => url_text(&saved),
                    _ => no_proxy_text(&saved),
                };
                (input.read(cx).value() != want).then(|| (input.clone(), want))
            })
            .collect();
        for (input, want) in stale {
            input.update(cx, |state, cx| state.set_value(want, window, cx));
        }
        self.recheck_proxy(cx);
        self.mask_proxy_url(&saved, window, cx);
    }

    /// Masks the URL field while it holds credentials typed in (the saved ones are already
    /// masked in its text). Set only when that changes, so the field's eye can still show them.
    fn mask_proxy_url(
        &mut self,
        saved: &ProxySetting,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.settings_page.network.input(NetworkStop::Url).cloned() else {
            return;
        };
        let text = input.read(cx).value().to_string();
        let masked = has_credentials(&text) && text.trim() != url_text(saved);
        let network = &mut self.settings_page.network;
        if network.url_masked != Some(masked) {
            network.url_masked = Some(masked);
            input.update(cx, |state, cx| state.set_masked(masked, window, cx));
        }
    }

    /// Sets `stop`'s field to `text`, as typing it would: the palette's and automation's way in.
    fn set_network_text(
        &mut self,
        stop: NetworkStop,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings_page.open || self.settings_page.network.applying() {
            return;
        }
        if let Some(input) = self.settings_page.network.input(stop).cloned() {
            input.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        self.settings_page.network.typed.insert(stop);
        self.recheck_proxy(cx);
    }

    /// Drops what was typed in the proxy fields: they show the saved proxy again.
    pub(super) fn revert_network_fields(&mut self) {
        let network = &mut self.settings_page.network;
        if !network.applying() {
            network.typed.clear();
            network.resync = true;
        }
    }

    /// Enter or Space on a control of the section.
    pub(super) fn activate_network_stop(
        &mut self,
        stop: NetworkStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match stop {
            NetworkStop::Url | NetworkStop::NoProxy => self.focus_settings_stop(window, cx),
            NetworkStop::Apply => self.ask_apply_proxy(window, cx),
        }
    }

    /// Asks before applying the proxy the fields propose, saying what reconnects and what ends.
    fn ask_apply_proxy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(setting) = self.proxy_to_apply(cx) else {
            return;
        };
        if confirm::any_open(window, cx) {
            return;
        }
        let saved = self.saved_proxy(cx);
        let text = DialogText {
            title: APPLY_TITLE.into(),
            text: impact_text(&setting, self.running_forwards(cx), self.live_sessions(cx)),
            ok: "Apply and Reconnect".into(),
            cancel: "Cancel".into(),
            id: "settings-proxy-description",
            // Port-forwards and sessions that end are not got back: Enter cancels.
            default_cancel: true,
        };
        let workspace = cx.weak_entity();
        confirm::open(window, cx, text, move |_, cx| {
            let (setting, saved) = (setting.clone(), saved.clone());
            let _ = workspace.update(cx, |ws, cx| ws.apply_proxy_confirmed(setting, saved, cx));
        });
    }

    /// Applies `setting`, confirmed over `saved`, unless either changed meanwhile.
    fn apply_proxy_confirmed(
        &mut self,
        setting: ProxySetting,
        saved: ProxySetting,
        cx: &mut Context<Self>,
    ) {
        let unchanged = self.proxy_to_apply(cx).is_some_and(|now| now == setting)
            && self.saved_proxy(cx) == saved;
        if !unchanged {
            self.settings_page.network.progress = Some(Progress::Failed(APPLY_STALE.into()));
            cx.notify();
            return;
        }
        let reply = self.model.read(cx).core().apply_proxy(setting.clone());
        let task = cx.spawn(async move |this, cx| {
            let applied = match reply.await {
                Ok(applied) => applied,
                Err(why) => {
                    let _ = this.update(cx, |ws, cx| {
                        ws.settings_page.network.progress = Some(Progress::Failed(why));
                        cx.notify();
                    });
                    return;
                }
            };
            let ProxyApplied {
                reconnecting,
                forwards_ended,
                settled,
                ..
            } = applied;
            let _ = this.update(cx, |ws, cx| {
                let network = &mut ws.settings_page.network;
                network.applied = Some(setting);
                network.progress = Some(Progress::Reconnecting {
                    waiting: reconnecting,
                    forwards_ended,
                });
                cx.notify();
            });
            let progress = match settled.await {
                Ok(report) => Progress::Done {
                    report,
                    forwards_ended,
                },
                Err(_) => Progress::Failed(
                    "the proxy was applied, but how the clusters reconnected is not known".into(),
                ),
            };
            let _ = this.update(cx, |ws, cx| {
                ws.settings_page.network.progress = Some(progress);
                cx.notify();
            });
        });
        let network = &mut self.settings_page.network;
        network.typed_while_applying = false;
        network.progress = Some(Progress::Applying);
        network.task = Some(task);
        cx.notify();
    }
}
