//! The main window: cluster sidebar on the left, the active cluster on the right.

use flint_core::kubeconfig::error_summary;
use flint_core::settings::AiExposureState;
use flint_core::{ClusterSummary, ConnectionState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _, Theme,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::clusters::{ClustersModel, state_label};
use super::display;

const SIDEBAR_WIDTH: f32 = 260.;

pub struct Workspace {
    model: Entity<ClustersModel>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(model: Entity<ClustersModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Follow macOS light and dark mode, now and whenever it changes.
        Theme::sync_system_appearance(Some(window), cx);
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
        });
        let observe = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            _subscriptions: vec![observe, appearance],
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let model = self.model.read(cx);
        let active = model.active.clone();

        let mut list = v_flex().gap_0p5().p_2();
        if let Some(error) = &model.settings_error {
            list = list.child(banner(
                "settings-error",
                "settings.toml is invalid. Nothing is exposed to AI until it is fixed.",
                error,
                &theme,
            ));
        }
        if !model.loaded {
            list = list.child(
                div()
                    .px_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Loading kubeconfig…"),
            );
        } else if model.clusters.is_empty() {
            list = list.child(empty_state(model, &theme));
        }
        for cluster in &model.clusters {
            let is_active = active.as_deref() == Some(cluster.info.name.as_str());
            let exposure = model.ai_exposure(cluster);
            let settings_ok = model.settings_error.is_none();
            list = list.child(self.render_cluster_row(
                cluster,
                is_active,
                exposure,
                settings_ok,
                &theme,
            ));
        }
        for (i, (path, error)) in model.kubeconfig_errors.iter().enumerate() {
            let detail = format!("{}\n\n{error}", path.display());
            list = list.child(
                div()
                    .id(SharedString::from(format!("kubeconfig-error-{i}")))
                    .px_2()
                    .pt_2()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(format!(
                        "{}: {}",
                        display::file_name(path),
                        error_summary(error)
                    ))
                    .tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx)),
            );
        }
        if let Some(error) = &model.last_error {
            list = list.child(
                div()
                    .px_2()
                    .pt_2()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }

        v_flex()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .child("CLUSTERS"),
            )
            .child(
                div()
                    .id("cluster-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(list),
            )
    }

    fn render_cluster_row(
        &self,
        cluster: &ClusterSummary,
        is_active: bool,
        exposure: AiExposureState,
        settings_ok: bool,
        theme: &Theme,
    ) -> AnyElement {
        let name = cluster.info.name.clone();
        let server = cluster.info.server.clone();
        let title = display::short_name(&name);
        let subtitle = display::subtitle(&name, cluster.info.server_host());
        let state_text = match &cluster.state {
            ConnectionState::Connected(v) => format!("Connected, Kubernetes {}", v.git_version),
            ConnectionState::Failed(message) => format!("Connection failed: {message}"),
            other => state_label(other).to_owned(),
        };
        let exposure_text = match &exposure {
            AiExposureState::Hidden => "Hidden from AI agents".to_owned(),
            AiExposureState::Exposed => "Exposed to AI agents (flint mcp, read-only)".to_owned(),
            AiExposureState::ServerChanged { exposed_server } => format!(
                "Cluster changed: exposed at {exposed_server}, now points elsewhere. Hidden from \
                 AI agents until you expose it again."
            ),
        };
        let hover_text = format!(
            "{}\n{}\n{}\n{}\nfrom {}",
            name,
            server.as_deref().unwrap_or("no server"),
            state_text,
            exposure_text,
            cluster.info.source.display()
        );
        let dot = match cluster.state {
            ConnectionState::Disconnected => theme.muted_foreground.opacity(0.4),
            ConnectionState::Connecting => theme.warning,
            ConnectionState::Connected(_) => theme.success,
            ConnectionState::Failed(_) => theme.danger,
        };

        let row_model = self.model.clone();
        let row_name = name.clone();

        h_flex()
            .id(SharedString::from(format!("cluster-{name}")))
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .cursor_pointer()
            .when(is_active, |el| {
                el.bg(theme.sidebar_accent)
                    .text_color(theme.sidebar_accent_foreground)
            })
            .when(!is_active, |el| {
                el.hover(|el| el.bg(theme.sidebar_accent.opacity(0.5)))
            })
            .tooltip(move |window, cx| Tooltip::new(hover_text.clone()).build(window, cx))
            .on_click(move |_, _, cx| {
                row_model.update(cx, |m, _| m.activate(row_name.clone()));
            })
            .child(div().size_2().flex_none().rounded_full().bg(dot))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(title))
                    .child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(subtitle),
                    ),
            )
            .when(exposure.is_exposed(), |row| {
                row.child(Tag::primary().xsmall().flex_none().child("AI"))
            })
            .when(
                matches!(exposure, AiExposureState::ServerChanged { .. }),
                |row| row.child(Tag::warning().xsmall().flex_none().child("Cluster changed")),
            )
            .child(self.render_exposure_toggle(&name, server, exposure, settings_ok))
            .into_any_element()
    }

    /// The Expose to AI button. Exposing asks for confirmation naming the context and server;
    /// hiding does not. It is not a tab stop, so a stray Space or Enter cannot flip it.
    fn render_exposure_toggle(
        &self,
        name: &str,
        server: Option<String>,
        exposure: AiExposureState,
        settings_ok: bool,
    ) -> Button {
        let model = self.model.clone();
        let context = name.to_owned();
        let button = Button::new(SharedString::from(format!("expose-{name}")))
            .icon(IconName::Bot)
            .xsmall()
            .tab_stop(false);
        if !settings_ok {
            return button
                .ghost()
                .disabled(true)
                .tooltip("settings.toml is invalid; fix it to change AI exposure.");
        }
        let Some(server) = server else {
            if matches!(exposure, AiExposureState::ServerChanged { .. }) {
                // A stale entry for a context that lost its server: it can only be removed.
                return button
                    .ghost()
                    .tooltip("Cluster changed: this context has no API server now. Click to remove its old AI exposure.")
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        model.update(cx, |m, _| m.hide_from_ai(context.clone()));
                    });
            }
            return button
                .ghost()
                .disabled(true)
                .tooltip("This context has no API server, so it cannot be exposed to AI.");
        };
        match exposure {
            AiExposureState::Exposed => button
                .primary()
                .tooltip("Exposed to AI agents (flint mcp, read-only). Click to hide.")
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    model.update(cx, |m, _| m.hide_from_ai(context.clone()));
                }),
            AiExposureState::Hidden | AiExposureState::ServerChanged { .. } => {
                let previous = match &exposure {
                    AiExposureState::ServerChanged { exposed_server } => {
                        Some(exposed_server.clone())
                    }
                    _ => None,
                };
                button
                    .ghost()
                    .tooltip(if previous.is_some() {
                        "Cluster changed: this context now points at a different server. \
                         Click to review and expose it again."
                    } else {
                        "Hidden from AI agents. Click to expose it to flint mcp (read-only)."
                    })
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        confirm_expose(
                            window,
                            cx,
                            model.clone(),
                            context.clone(),
                            server.clone(),
                            previous.clone(),
                        );
                    })
            }
        }
    }

    fn render_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let model = self.model.read(cx);
        let content = match model.active_cluster() {
            None => v_flex()
                .gap_1()
                .items_center()
                .child(div().text_lg().child("Select a cluster"))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Flint connects only when you pick one in the sidebar."),
                ),
            Some(cluster) => {
                let detail: AnyElement = match &cluster.state {
                    ConnectionState::Disconnected | ConnectionState::Connecting => div()
                        .text_color(theme.muted_foreground)
                        .child("Connecting…")
                        .into_any_element(),
                    ConnectionState::Connected(version) => div()
                        .child(format!(
                            "Connected. Kubernetes {} ({})",
                            version.git_version, version.platform
                        ))
                        .into_any_element(),
                    ConnectionState::Failed(message) => v_flex()
                        .gap_1()
                        .max_w(px(640.))
                        .child(div().text_color(theme.danger).child("Connection failed"))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(message.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Select the cluster again to retry."),
                        )
                        .into_any_element(),
                };
                let full_name = cluster.info.name.clone();
                v_flex()
                    .gap_2()
                    .items_center()
                    .max_w_full()
                    .child(
                        div()
                            .id("cluster-title")
                            .max_w_full()
                            .text_xl()
                            .truncate()
                            .child(display::short_name(&cluster.info.name))
                            .tooltip(move |window, cx| {
                                Tooltip::new(full_name.clone()).build(window, cx)
                            }),
                    )
                    .child(
                        div()
                            .max_w_full()
                            .text_sm()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} · {}",
                                cluster.info.server.as_deref().unwrap_or("no server"),
                                state_label(&cluster.state)
                            )),
                    )
                    .child(detail)
            }
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .justify_center()
            .p_6()
            .overflow_hidden()
            .child(content)
    }
}

/// Asks before exposing a cluster to AI agents, naming the context and the server it points at.
fn confirm_expose(
    window: &mut Window,
    cx: &mut App,
    model: Entity<ClustersModel>,
    context: String,
    server: String,
    previous: Option<String>,
) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let mut description = format!(
            "AI agents using flint mcp will be able to read this cluster: resources, events and \
             pod logs. They cannot change anything, and Secret values stay redacted.\n\n\
             Context: {context}\nServer: {server}"
        );
        if let Some(previous) = &previous {
            description.push_str(&format!(
                "\n\nThis context was exposed at {previous}, which is a different server."
            ));
        }
        let (model, context, server) = (model.clone(), context.clone(), server.clone());
        alert
            .confirm()
            .title("Expose to AI agents?")
            .description(description)
            .ok_text("Expose")
            .cancel_text("Cancel")
            .on_ok(move |_, _, cx| {
                model.update(cx, |m, _| m.expose_to_ai(context.clone(), server.clone()));
                true
            })
    });
}

/// A warning shown at the top of the sidebar, with the full detail in a tooltip.
fn banner(id: &'static str, message: &'static str, detail: &str, theme: &Theme) -> AnyElement {
    let detail = detail.to_owned();
    div()
        .id(id)
        .mx_1()
        .mb_2()
        .px_2()
        .py_1p5()
        .rounded_md()
        .border_1()
        .border_color(theme.danger)
        .bg(theme.danger.opacity(0.1))
        .text_xs()
        .text_color(theme.danger)
        .child(message)
        .child(
            div()
                .text_color(theme.muted_foreground)
                .truncate()
                .child(error_summary(&detail)),
        )
        .tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx))
        .into_any_element()
}

/// What to show when no contexts were found: the files searched and the `KUBECONFIG` value.
fn empty_state(model: &ClustersModel, theme: &Theme) -> AnyElement {
    let kubeconfig_env = match &model.kubeconfig_env {
        Some(v) if !v.is_empty() => v.to_string_lossy().into_owned(),
        Some(_) => "(set but empty)".to_owned(),
        None => "(not set)".to_owned(),
    };
    v_flex()
        .px_2()
        .gap_1()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child("No contexts found in your kubeconfig files.")
        .child(
            div()
                .text_xs()
                .pt_1()
                .child(format!("KUBECONFIG: {kubeconfig_env}")),
        )
        .child(div().text_xs().child("Searched:"))
        .children(model.kubeconfig_paths.iter().map(|path| {
            let exists = path.exists();
            div().text_xs().pl_2().child(format!(
                "{}{}",
                path.display(),
                if exists { "" } else { " (missing)" }
            ))
        }))
        .when(model.kubeconfig_paths.is_empty(), |el| {
            el.child(div().text_xs().pl_2().child("(no paths: HOME is not set)"))
        })
        .into_any_element()
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        h_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
    }
}
