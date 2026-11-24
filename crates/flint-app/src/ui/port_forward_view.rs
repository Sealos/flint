//! The port-forward panel (UP-70, UP-73, UP-74): a dock tab per cluster with a form for the
//! selected pod or service and the list of its forwards, each with its local address, state and
//! Stop and Open in Browser buttons.
//!
//! The core owns the forwarding and its gate ([`flint_core::port_forward`]). A row holds its
//! forward, so closing the tab or the window, or quitting, stops it. The core ends a forward
//! when a change of cluster, connection or keep watching calls for it, and its row says so.

use std::sync::atomic::{AtomicU64, Ordering};

use flint_core::handle::AdmittedMember;
use flint_core::port_forward::{
    DeclaredPort, ForwardRequest, ForwardSnapshot, ForwardState, ForwardTarget, PortForward,
    browser_url,
};
use flint_core::{ClusterId, CoreHandle};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::dock::{DockItem, TabKey, TabStatus};
use super::registry::{self, OpenForwardedPort, StartPortForward, StopPortForward};

/// What the panel says before anything is forwarded.
pub const EMPTY: &str =
    "No port forwards. Select a pod or service and choose Forward Port from its menu.";
/// The most rows a panel keeps, so a long session's ended forwards stay bounded.
const MAX_ROWS: usize = 32;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Why a Forward Port did not open.
pub fn refused(why: &str) -> String {
    format!(
        "Can't forward a port: {}.",
        why.trim_end().trim_end_matches('.')
    )
}

/// A typed port: `Ok(None)` when left empty.
pub fn parse_port(text: &str) -> Result<Option<u16>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<u16>() {
        Ok(port) if port > 0 => Ok(Some(port)),
        _ => Err(format!("\"{text}\" is not a port from 1 to 65535")),
    }
}

/// A forward's state in words.
pub fn state_text(snapshot: &ForwardSnapshot) -> String {
    match &snapshot.state {
        ForwardState::Starting => "Starting".to_owned(),
        ForwardState::Listening => match &snapshot.warning {
            Some(warning) => format!("Listening, but {warning}"),
            None => match &snapshot.pod {
                Some(pod) => format!("Listening, via {pod}"),
                None => "Listening".to_owned(),
            },
        },
        ForwardState::Stopping => "Stopping".to_owned(),
        ForwardState::Failed(failure) => format!("Failed: {failure}"),
        ForwardState::Stopped => "Stopped".to_owned(),
    }
}

/// What the form forwards to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormTarget {
    pub namespace: String,
    pub target: ForwardTarget,
    /// For a pod, the uid of the one selected.
    pub uid: Option<String>,
    pub ports: Vec<DeclaredPort>,
}

/// The connection a form's target was selected on: what its forward starts through.
#[derive(Clone)]
pub struct FormBinding {
    pub core: CoreHandle,
    pub server: Option<String>,
    pub member: Option<AdmittedMember>,
}

/// The ports a target declares, as the form's hint says them.
fn ports_hint(ports: &[DeclaredPort]) -> String {
    let named: Vec<String> = ports
        .iter()
        .map(|p| match &p.name {
            Some(name) => format!("{} ({name})", p.port),
            None => p.port.to_string(),
        })
        .collect();
    if named.is_empty() {
        "It declares no ports: type the one to reach.".to_owned()
    } else {
        format!("Declared: {}", named.join(", "))
    }
}

struct Row {
    id: u64,
    forward: PortForward,
    declared: Option<DeclaredPort>,
    shown: ForwardSnapshot,
    _updates: Task<()>,
}

impl Row {
    fn url(&self) -> Option<String> {
        // The live state, not the row's copy: the port may be on its way to being released.
        let now = self.forward.snapshot();
        if now.state != ForwardState::Listening {
            return None;
        }
        let declared = self.declared.as_ref();
        Some(browser_url(
            now.local_port?,
            declared.and_then(|p| p.name.as_deref()),
            declared.and_then(|p| p.app_protocol.as_deref()),
        ))
    }

    /// Target, ports and state, as one line for the row's accessible name.
    fn label(&self) -> String {
        let request = self.forward.request();
        let local = self
            .shown
            .local_port
            .map_or("…".to_owned(), |p| format!("127.0.0.1:{p}"));
        format!(
            "{}/{} {local} to {}: {}",
            request.namespace,
            request.target,
            request.remote_port,
            state_text(&self.shown)
        )
    }
}

pub struct PortForwardsView {
    cluster: ClusterId,
    focus: FocusHandle,
    form: Option<FormTarget>,
    /// Set and cleared with `form`.
    binding: Option<FormBinding>,
    remote: Entity<InputState>,
    local: Entity<InputState>,
    error: Option<String>,
    rows: Vec<Row>,
    _inputs: [Subscription; 2],
}

impl PortForwardsView {
    pub fn new(cluster: ClusterId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let remote = cx.new(|cx| InputState::new(window, cx).placeholder("Port"));
        let local = cx.new(|cx| InputState::new(window, cx).placeholder("Any free port"));
        // Through the action, so Enter takes the workspace's checks as the button does.
        let on_enter = |_: &mut Self,
                        _: &Entity<InputState>,
                        event: &InputEvent,
                        window: &mut Window,
                        cx: &mut Context<Self>| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                window.dispatch_action(Box::new(StartPortForward), cx);
            }
        };
        let inputs = [
            cx.subscribe_in(&remote, window, on_enter),
            cx.subscribe_in(&local, window, on_enter),
        ];
        PortForwardsView {
            cluster,
            focus: cx.focus_handle(),
            form: None,
            binding: None,
            remote,
            local,
            error: None,
            rows: Vec::new(),
            _inputs: inputs,
        }
    }

    /// Points the form at `target`, selected on `binding`, with its first declared port filled
    /// in, and focuses it.
    pub fn set_target(
        &mut self,
        target: FormTarget,
        binding: FormBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let first = target.ports.first().map(|p| p.port.to_string());
        self.remote.update(cx, |input, cx| {
            input.set_value(first.unwrap_or_default(), window, cx);
            input.focus(window, cx);
        });
        self.local
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.form = Some(target);
        self.binding = Some(binding);
        self.error = None;
        cx.notify();
    }

    /// Whether the form's target was selected on `member` (`None`: outside a set).
    pub fn form_bound_to(&self, member: Option<&AdmittedMember>) -> bool {
        self.binding
            .as_ref()
            .is_some_and(|binding| binding.member.as_ref() == member)
    }

    /// Forgets the form's target: it has to be selected again.
    pub fn clear_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        self.binding = None;
        cx.notify();
    }

    /// The context this panel's forwards run on.
    pub fn tab_cluster(&self) -> &ClusterId {
        &self.cluster
    }

    pub fn can_start(&self) -> bool {
        self.form.is_some()
    }

    /// Starts a forward from the form. A wrong port stays in the form, saying why.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        let (Some(form), Some(binding)) = (self.form.clone(), self.binding.clone()) else {
            return;
        };
        let typed = parse_port(&self.remote.read(cx).value()).and_then(|remote| {
            let remote = remote.ok_or("type the port to reach")?;
            Ok((remote, parse_port(&self.local.read(cx).value())?))
        });
        let (remote_port, local_port) = match typed {
            Ok(ports) => ports,
            Err(why) => {
                self.error = Some(why);
                cx.notify();
                return;
            }
        };
        if !self.make_room() {
            self.error = Some("too many forwards: stop one first".to_owned());
            cx.notify();
            return;
        }
        self.error = None;
        let declared = form.ports.iter().find(|p| p.port == remote_port).cloned();
        let forward = binding.core.port_forward(ForwardRequest {
            cluster: self.cluster.clone(),
            namespace: form.namespace,
            target: form.target,
            remote_port,
            local_port,
            server: Some(binding.server),
            uid: form.uid,
        });
        self.add_row(forward, declared, cx);
    }

    /// Forgets the oldest ended row if the panel is full; `false` if all are running.
    fn make_room(&mut self) -> bool {
        if self.rows.len() < MAX_ROWS {
            return true;
        }
        match self.rows.iter().position(|r| r.shown.state.is_finished()) {
            Some(ix) => {
                self.rows.remove(ix);
                true
            }
            None => false,
        }
    }

    fn add_row(
        &mut self,
        forward: PortForward,
        declared: Option<DeclaredPort>,
        cx: &mut Context<Self>,
    ) {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut updates = forward.updates();
        let shown = forward.snapshot();
        let task = cx.spawn(async move |this, cx| {
            while updates.changed().await {
                let snapshot = updates.latest();
                let kept = this.update(cx, |view, cx| {
                    if let Some(row) = view.rows.iter_mut().find(|r| r.id == id) {
                        row.shown = snapshot;
                        cx.notify();
                    }
                });
                if kept.is_err() {
                    return;
                }
            }
        });
        self.rows.push(Row {
            id,
            forward,
            declared,
            shown,
            _updates: task,
        });
        cx.notify();
    }

    pub fn has_row(&self, id: u64) -> bool {
        self.rows.iter().any(|r| r.id == id)
    }

    /// Takes `forward` as a row, as Start would; its id.
    #[cfg(test)]
    pub fn adopt_for_tests(&mut self, forward: PortForward, cx: &mut Context<Self>) -> u64 {
        self.add_row(forward, None, cx);
        self.rows.last().expect("just added").id
    }

    /// What the form forwards to, as it shows it.
    #[cfg(test)]
    pub fn form_target(&self) -> Option<String> {
        let form = self.form.as_ref()?;
        Some(format!("{}/{}", form.namespace, form.target))
    }

    /// The port typed in the form.
    #[cfg(test)]
    pub fn remote_text(&self, cx: &App) -> String {
        self.remote.read(cx).value().to_string()
    }

    /// Stops forward `id` and takes its row away.
    pub fn stop(&mut self, id: u64, cx: &mut Context<Self>) {
        self.rows.retain(|r| r.id != id);
        cx.notify();
    }

    /// Stops every forward. The rows stay and say so.
    pub fn stop_all(&mut self, cx: &mut Context<Self>) {
        for row in &self.rows {
            row.forward.stop();
        }
        cx.notify();
    }

    pub fn has_running(&self) -> bool {
        self.rows.iter().any(|r| !r.shown.state.is_finished())
    }

    /// The address of forward `id`, while it listens.
    pub fn url(&self, id: u64) -> Option<String> {
        self.rows.iter().find(|r| r.id == id)?.url()
    }

    /// Each row's line, as the window's state reports it.
    #[cfg(any(test, feature = "automation"))]
    pub fn row_labels(&self) -> Vec<String> {
        self.rows.iter().map(Row::label).collect()
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let form = self.form.as_ref();
        h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .flex_wrap()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .child(
                div()
                    .id("forward-target")
                    .child(form.map_or("No target selected".to_owned(), |f| {
                        format!("{}/{}", f.namespace, f.target)
                    }))
                    .test_support(),
            )
            .child("Port")
            .child(
                div()
                    .id("forward-remote-field")
                    .w(px(90.))
                    .aria_label("Port on the target")
                    .child(Input::new(&self.remote).id("forward-remote").small()),
            )
            .child("Local port")
            .child(
                div()
                    .id("forward-local-field")
                    .w(px(120.))
                    .aria_label("Local port, empty for any free port")
                    .child(Input::new(&self.local).id("forward-local").small()),
            )
            .child(
                Button::new("forward-start")
                    .primary()
                    .xsmall()
                    .label("Forward")
                    .disabled(form.is_none())
                    .on_click(registry::on_click_in(self.focus.clone(), StartPortForward)),
            )
            .when_some(form, |el, f| {
                el.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(ports_hint(&f.ports)),
                )
            })
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .id("forward-error")
                        .aria_label(error.clone())
                        .text_color(theme.danger)
                        .child(error)
                        .test_support(),
                )
            })
    }

    fn render_row(&self, row: &Row, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let id = row.id;
        let request = row.forward.request();
        let failed = matches!(row.shown.state, ForwardState::Failed(_));
        let warned = row.shown.warning.is_some();
        let finished = row.shown.state.is_finished();
        let local = row
            .shown
            .local_port
            .map_or("…".to_owned(), |p| format!("127.0.0.1:{p}"));
        h_flex()
            .id(("forward-row", id))
            .role(Role::Row)
            .aria_label(row.label())
            .px_3()
            .py_1()
            .gap_3()
            .text_xs()
            .child(format!("{}/{}", request.namespace, request.target))
            .child(format!("{local} → {}", request.remote_port))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when(failed, |el| el.text_color(theme.danger))
                    .when(warned && !failed, |el| el.text_color(theme.warning))
                    .when(finished && !failed, |el| {
                        el.text_color(theme.muted_foreground)
                    })
                    .child(state_text(&row.shown)),
            )
            .when(row.url().is_some(), |el| {
                el.child(
                    Button::new(("forward-open", id))
                        .ghost()
                        .xsmall()
                        .label("Open in Browser")
                        .on_click(registry::on_click_in(
                            self.focus.clone(),
                            OpenForwardedPort { id },
                        )),
                )
            })
            .child(
                Button::new(("forward-stop", id))
                    .ghost()
                    .xsmall()
                    .label(if finished { "Dismiss" } else { "Stop" })
                    .on_click(registry::on_click_in(
                        self.focus.clone(),
                        StopPortForward { id },
                    )),
            )
            .test_support()
    }
}

impl DockItem for PortForwardsView {
    fn tab_key(&self) -> TabKey {
        TabKey::Forwards(self.cluster.clone())
    }

    fn tab_title(&self) -> SharedString {
        "Port Forwards".into()
    }

    fn tab_tooltip(&self) -> SharedString {
        format!(
            "Port forwards on {}. They listen on 127.0.0.1 only. Closing this tab stops them.",
            self.cluster
        )
        .into()
    }

    fn tab_status(&self) -> TabStatus {
        if self.has_running() {
            TabStatus::Live
        } else if self
            .rows
            .iter()
            .any(|r| matches!(r.shown.state, ForwardState::Failed(_)))
        {
            TabStatus::Failed
        } else {
            TabStatus::Ended
        }
    }

    fn tab_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn can_restart(&self) -> bool {
        false
    }
}

impl Render for PortForwardsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let form = self.render_form(cx).into_any_element();
        let rows: Vec<AnyElement> = self
            .rows
            .iter()
            .map(|row| self.render_row(row, cx).into_any_element())
            .collect();
        v_flex()
            .id("port-forwards")
            .track_focus(&self.focus)
            .size_full()
            .child(form)
            .when(rows.is_empty(), |el| {
                el.child(
                    div()
                        .id("forwards-empty")
                        .p_3()
                        .text_xs()
                        .text_color(muted)
                        .aria_label(EMPTY)
                        .child(EMPTY)
                        .test_support(),
                )
            })
            .child(
                v_flex()
                    .id("forwards-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}

#[cfg(test)]
mod tests {
    use flint_core::port_forward::{DeclaredPort, ForwardSnapshot, ForwardState};

    use super::{parse_port, ports_hint, refused, state_text};

    fn snapshot(state: ForwardState) -> ForwardSnapshot {
        ForwardSnapshot {
            state,
            local_port: Some(8080),
            pod: None,
            connections: 0,
            warning: None,
        }
    }

    // covers UP-73.
    #[test]
    fn a_typed_port_is_a_number_from_1_to_65535_or_empty() {
        assert_eq!(parse_port("  "), Ok(None));
        assert_eq!(parse_port(" 8080 "), Ok(Some(8080)));
        assert_eq!(parse_port("65535"), Ok(Some(65535)));
        for wrong in ["0", "65536", "-1", "http", "80 81"] {
            assert!(parse_port(wrong).is_err(), "{wrong}");
        }
    }

    // covers UP-70: the state says what happened.
    #[test]
    fn the_state_says_a_drop_and_a_failure() {
        let mut shown = snapshot(ForwardState::Listening);
        assert_eq!(state_text(&shown), "Listening");
        shown.pod = Some("web-1".into());
        assert_eq!(state_text(&shown), "Listening, via web-1");
        shown.warning = Some("a connection to port 80 dropped".into());
        assert_eq!(
            state_text(&shown),
            "Listening, but a connection to port 80 dropped"
        );
        assert_eq!(state_text(&snapshot(ForwardState::Stopped)), "Stopped");
    }

    #[test]
    fn the_hint_names_declared_ports() {
        let ports = [
            DeclaredPort {
                port: 80,
                name: Some("http".into()),
                app_protocol: None,
            },
            DeclaredPort {
                port: 9000,
                name: None,
                app_protocol: None,
            },
        ];
        assert_eq!(ports_hint(&ports), "Declared: 80 (http), 9000");
        assert!(ports_hint(&[]).contains("no ports"));
    }

    #[test]
    fn a_refusal_reads_as_one_sentence() {
        assert_eq!(
            refused("alpha is protected."),
            "Can't forward a port: alpha is protected."
        );
    }
}
