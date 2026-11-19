//! The generic detail view (#10): Overview (metadata, labels, annotations, owners, conditions,
//! events, and Secret data masked per key) and read-only YAML with syntax highlighting.
//!
//! The object is fetched through the core ([`CoreHandle::fetch_object`]) and fetched again when
//! the list reports that its resource version moved.

use std::collections::HashSet;
use std::sync::Arc;

use flint_core::read::event_time;
use flint_core::table::{human_duration, unix_now};
use flint_core::{ClusterId, CoreHandle, KindId, ObjectDetail};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, Theme, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::list::tabular_figures;
use super::log_view::LogRequest;
use super::notice::notice;
use super::object::{self, MASK, Overview, PodContainer, SecretValue};
use super::registry::{
    self, Close, OpenContainerLogs, OpenContainerShell, OpenLogs, OpenShell, Refresh, ShowOverview,
    ShowYaml, ToggleAnnotations, ToggleLabels, ToggleManagedFields, ToggleSecretValue,
};
use super::terminal_model::ShellRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Overview,
    Yaml,
}

/// Identifies the object a detail view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    pub cluster: ClusterId,
    pub kind: KindId,
    /// What to call the kind ("Deployments" for a list, the singular from the object itself).
    pub kind_label: String,
    pub namespace: Option<String>,
    pub name: String,
    pub uid: Box<str>,
}

enum Fetch {
    Loading,
    Loaded {
        detail: Arc<ObjectDetail>,
        /// Shared, so a render borrows it instead of copying labels, annotations and events.
        overview: Arc<Overview>,
    },
    Failed(String),
}

pub enum DetailEvent {
    Close,
    /// Open (or show) a log tab for one of the pod's containers.
    OpenLogs(LogRequest),
    /// Open (or show) a shell tab in one of the pod's containers.
    OpenShell(ShellRequest),
}

impl EventEmitter<DetailEvent> for DetailView {}

pub struct DetailView {
    core: CoreHandle,
    object: ObjectRef,
    is_secret: bool,
    tab: DetailTab,
    fetch: Fetch,
    /// A fetch is running.
    fetching: bool,
    /// The object changed while a fetch ran: fetch again when it ends.
    refetch: bool,
    /// The last fetch failed while an older copy is still shown.
    refresh_error: Option<String>,
    /// The object disappeared from the list (deleted).
    gone: bool,
    revealed: HashSet<String>,
    show_annotations: bool,
    show_labels: bool,
    yaml: Entity<EditorState>,
    yaml_text: String,
    /// Show `metadata.managedFields` in the YAML (hidden by default).
    show_managed_fields: bool,
    /// Open the pod's logs once it is fetched (`FLINT_VIEW=…#logs`).
    open_logs_when_loaded: bool,
    _fetch: Task<()>,
    _yaml: Task<()>,
}

impl DetailView {
    pub fn new(
        core: CoreHandle,
        object: ObjectRef,
        tab: DetailTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let yaml = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("yaml")
                .line_number(true)
                .soft_wrap(false)
        });
        let is_secret = object.kind.as_str() == "secrets";
        let mut view = DetailView {
            core,
            object,
            is_secret,
            tab,
            fetch: Fetch::Loading,
            fetching: false,
            refetch: false,
            refresh_error: None,
            gone: false,
            revealed: HashSet::new(),
            show_annotations: false,
            show_labels: true,
            yaml,
            yaml_text: String::new(),
            show_managed_fields: false,
            open_logs_when_loaded: false,
            _fetch: Task::ready(()),
            _yaml: Task::ready(()),
        };
        view.refresh(window, cx);
        view
    }

    pub fn object(&self) -> &ObjectRef {
        &self.object
    }

    pub fn tab(&self) -> DetailTab {
        self.tab
    }

    /// Opens the pod's logs as soon as it is fetched, as Logs would (`FLINT_VIEW=…#logs`).
    pub fn open_logs_when_loaded(&mut self) {
        self.open_logs_when_loaded = true;
    }

    /// Fetches the object again. Calls while a fetch runs coalesce into one more fetch.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.fetching {
            self.refetch = true;
            return;
        }
        self.fetching = true;
        self.refetch = false;
        let o = &self.object;
        let fetch = self
            .core
            .fetch_object(&o.cluster, &o.kind, o.namespace.as_deref(), &o.name);
        let is_secret = self.is_secret;
        let managed = self.show_managed_fields;
        self._fetch = cx.spawn_in(window, async move |this, cx| {
            let result = fetch.await;
            // Reading the object and rendering YAML are not free for big objects: off the UI
            // thread.
            let prepared = cx
                .background_spawn(async move {
                    result.map(|detail| {
                        let overview = object::overview(&detail.object, is_secret);
                        let yaml = object::render_yaml(&detail, is_secret, managed);
                        (detail, Arc::new(overview), yaml)
                    })
                })
                .await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.fetching = false;
                match prepared {
                    Ok((detail, overview, yaml)) => {
                        view.refresh_error = None;
                        // A re-render of the previous copy must not land after this one.
                        view._yaml = Task::ready(());
                        view.fetch = Fetch::Loaded { detail, overview };
                        if std::mem::take(&mut view.open_logs_when_loaded) {
                            view.open_logs::<OpenLogs>(None, false, cx);
                        }
                        if view.show_managed_fields == managed {
                            view.set_yaml(yaml, window, cx);
                        } else {
                            // Toggled while this fetch ran.
                            view.rerender_yaml(window, cx);
                        }
                    }
                    Err(error) => match view.fetch {
                        Fetch::Loaded { .. } => view.refresh_error = Some(error),
                        _ => view.fetch = Fetch::Failed(error),
                    },
                }
                if view.refetch {
                    view.refresh(window, cx);
                }
                cx.notify();
            });
        });
    }

    fn set_yaml(&mut self, yaml: String, window: &mut Window, cx: &mut Context<Self>) {
        if yaml != self.yaml_text {
            self.yaml
                .update(cx, |state, cx| state.set_value(yaml.clone(), window, cx));
            self.yaml_text = yaml;
        }
    }

    /// Renders the YAML again (off the UI thread) after the managedFields toggle changed.
    fn rerender_yaml(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Fetch::Loaded { detail, .. } = &self.fetch else {
            return;
        };
        let detail = detail.clone();
        let (is_secret, managed) = (self.is_secret, self.show_managed_fields);
        self._yaml = cx.spawn_in(window, async move |this, cx| {
            let yaml = cx
                .background_spawn(async move { object::render_yaml(&detail, is_secret, managed) })
                .await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.show_managed_fields == managed {
                    view.set_yaml(yaml, window, cx);
                    cx.notify();
                }
            });
        });
    }

    fn toggle_managed_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_managed_fields = !self.show_managed_fields;
        self.rerender_yaml(window, cx);
        cx.notify();
    }

    /// The object was deleted: keep showing the last copy, marked as gone.
    pub fn mark_gone(&mut self, cx: &mut Context<Self>) {
        self.gone = true;
        cx.notify();
    }

    /// Shows `detail` as if a fetch had just returned it: the test servers never answer.
    #[cfg(test)]
    pub fn show_fetched(&mut self, detail: ObjectDetail, cx: &mut Context<Self>) {
        let overview = Arc::new(object::overview(&detail.object, self.is_secret));
        self.fetch = Fetch::Loaded {
            detail: Arc::new(detail),
            overview,
        };
        cx.notify();
    }

    fn render_header(&self, theme: &Theme, _cx: &mut Context<Self>) -> impl IntoElement {
        let selected = match self.tab {
            DetailTab::Overview => 0,
            DetailTab::Yaml => 1,
        };
        let kind = match &self.fetch {
            Fetch::Loaded { overview, .. } if !overview.kind.is_empty() => overview.kind.clone(),
            _ => self.object.kind_label.clone(),
        };
        let full_name = match &self.object.namespace {
            Some(ns) => format!("{ns}/{}", self.object.name),
            None => self.object.name.clone(),
        };
        v_flex()
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .h(px(super::workspace::HEADER_HEIGHT))
                    .px_3()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(kind),
                    )
                    .child(
                        div()
                            .id("detail-name")
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_semibold()
                            .font_family(theme.mono_font_family.clone())
                            .truncate()
                            .child(self.object.name.clone())
                            .tooltip(move |window, cx| {
                                Tooltip::new(full_name.clone()).build(window, cx)
                            }),
                    )
                    .when(self.fetching, |el| el.child(Spinner::new().xsmall()))
                    .children(self.render_logs_button())
                    .children(self.render_shell_button())
                    .child(
                        Button::new("detail-refresh")
                            .icon(IconName::RefreshCw)
                            .xsmall()
                            .ghost()
                            .tooltip("Fetch again")
                            .on_click(registry::on_click(Refresh)),
                    )
                    .child(
                        Button::new("detail-close")
                            .icon(IconName::Close)
                            .xsmall()
                            .ghost()
                            .tooltip("Close (Esc)")
                            .on_click(registry::on_click(Close)),
                    ),
            )
            .child(
                TabBar::new("detail-tabs")
                    .underline()
                    .small()
                    .px_2()
                    .selected_index(selected)
                    .child(Tab::new().label("Overview"))
                    .child(Tab::new().label("YAML"))
                    .on_click(|ix, window, cx| {
                        let shown = if *ix == 0 {
                            registry::perform(&ShowOverview, window, cx)
                        } else {
                            registry::perform(&ShowYaml, window, cx)
                        };
                        if let Err(e) = shown {
                            tracing::warn!("{e}");
                        }
                    }),
            )
    }

    /// For pods: Logs, with a container picker when the pod has several containers or a
    /// previous instance to read.
    fn render_logs_button(&self) -> Option<AnyElement> {
        let containers: Vec<PodContainer> = self.log_containers().to_vec();
        let button = Button::new("detail-logs")
            .xsmall()
            .ghost()
            .label("Logs")
            .tooltip(format!(
                "Open the logs in the dock ({})",
                registry::key_hint("secondary-l")
            ));
        match containers.as_slice() {
            [] => None,
            [only] if !only.has_previous => Some(
                button
                    .on_click(registry::on_click(OpenLogs))
                    .into_any_element(),
            ),
            _ => Some(
                button
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        let mut menu = menu.label("Container");
                        for c in &containers {
                            let label = if c.init {
                                format!("{} (init)", c.name)
                            } else {
                                c.name.clone()
                            };
                            menu = menu.item(PopupMenuItem::new(label).on_click(
                                registry::on_click(OpenContainerLogs {
                                    container: c.name.clone(),
                                    previous: false,
                                }),
                            ));
                        }
                        if containers.iter().any(|c| c.has_previous) {
                            menu = menu.separator().label("Previous instance");
                            for c in containers.iter().filter(|c| c.has_previous) {
                                menu = menu.item(PopupMenuItem::new(c.name.clone()).on_click(
                                    registry::on_click(OpenContainerLogs {
                                        container: c.name.clone(),
                                        previous: true,
                                    }),
                                ));
                            }
                        }
                        menu
                    })
                    .into_any_element(),
            ),
        }
    }

    /// For pods: Shell, with a container picker when the pod has several app containers.
    fn render_shell_button(&self) -> Option<AnyElement> {
        let containers: Vec<String> = self.shell_containers().map(|c| c.name.clone()).collect();
        let button = Button::new("detail-shell")
            .xsmall()
            .ghost()
            .label("Shell")
            .tooltip("Open a shell in the dock (⇧⌘E)");
        match containers.as_slice() {
            [] => None,
            [_] => Some(
                button
                    .on_click(registry::on_click(OpenShell))
                    .into_any_element(),
            ),
            _ => Some(
                button
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        let mut menu = menu.label("Container");
                        for name in &containers {
                            menu = menu.item(PopupMenuItem::new(name.clone()).on_click(
                                registry::on_click(OpenContainerShell {
                                    container: name.clone(),
                                }),
                            ));
                        }
                        menu
                    })
                    .into_any_element(),
            ),
        }
    }

    fn render_notices(&self, theme: &Theme) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        if self.gone {
            notices.push(notice(
                "detail-gone",
                "This object was deleted. Showing the last copy fetched.",
                theme.warning,
            ));
        }
        if let Some(error) = &self.refresh_error {
            notices.push(notice(
                "detail-refresh-error",
                format!("Could not fetch the latest version: {error}"),
                theme.danger,
            ));
        }
        if self.is_secret && self.tab == DetailTab::Yaml {
            notices.push(notice(
                "detail-secret-yaml",
                "Secret values are hidden here. Reveal them one by one in Overview.",
                theme.muted_foreground,
            ));
        }
        notices
            .into_iter()
            .map(IntoElement::into_any_element)
            .collect()
    }

    /// The YAML tab's bar: the managedFields toggle.
    fn render_yaml_bar(
        &self,
        detail: &ObjectDetail,
        theme: &Theme,
        _cx: &Context<Self>,
    ) -> impl IntoElement {
        let has_managed = detail.managed_fields.is_some();
        h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(div().flex_1().min_w_0().truncate().child(if !has_managed {
                "No managedFields on this object."
            } else if self.show_managed_fields {
                "Showing metadata.managedFields."
            } else {
                "metadata.managedFields is hidden."
            }))
            .when(has_managed, |el| {
                el.child(
                    Button::new("toggle-managed-fields")
                        .xsmall()
                        .ghost()
                        .label(if self.show_managed_fields {
                            "Hide managedFields"
                        } else {
                            "Show managedFields"
                        })
                        .tooltip("Which field manager (kubectl, Helm, a controller) set each field")
                        .on_click(registry::on_click(ToggleManagedFields)),
                )
            })
    }

    fn render_overview(
        &self,
        detail: &ObjectDetail,
        o: &Overview,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now = unix_now();
        let mut body = v_flex().gap_3().p_3().pb_6();

        // Metadata. Names and identifiers are monospace.
        let mut meta = Vec::new();
        meta.push(("Name", o.name.clone(), true));
        if let Some(ns) = &o.namespace {
            meta.push(("Namespace", ns.clone(), true));
        }
        meta.push(("Kind", format!("{} ({})", o.kind, o.api_version), false));
        if let Some(created) = &o.created {
            let age = timestamp_age(created, now)
                .map(|a| format!(" ({a} ago)"))
                .unwrap_or_default();
            meta.push(("Created", format!("{created}{age}"), false));
        }
        meta.push(("UID", o.uid.clone(), true));
        meta.push(("Resource version", o.resource_version.clone(), true));
        if let Some(generation) = o.generation {
            meta.push(("Generation", generation.to_string(), false));
        }
        if !o.finalizers.is_empty() {
            meta.push(("Finalizers", o.finalizers.join(", "), true));
        }
        body = body.child(section(
            "Metadata",
            None,
            v_flex().children(
                meta.into_iter()
                    .enumerate()
                    .map(|(i, (k, v, mono))| key_value(("meta", i), k, v, mono, theme)),
            ),
            theme,
        ));

        // Labels and annotations, collapsible.
        body = body.child(self.render_pairs(
            "Labels",
            &o.labels,
            self.show_labels,
            Box::new(ToggleLabels),
            theme,
        ));
        body = body.child(self.render_pairs(
            "Annotations",
            &o.annotations,
            self.show_annotations,
            Box::new(ToggleAnnotations),
            theme,
        ));

        if !o.owners.is_empty() {
            body = body.child(section(
                "Owners",
                Some(o.owners.len()),
                v_flex().gap_0p5().children(o.owners.iter().map(|owner| {
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(owner.kind.clone()),
                        )
                        .child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .child(owner.name.clone()),
                        )
                        .when(owner.controller, |el| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("controller"),
                            )
                        })
                })),
                theme,
            ));
        }

        if let Some(secret) = &o.secret {
            body = body.child(self.render_secret(secret, theme, cx));
        }

        if !o.conditions.is_empty() {
            body = body.child(section(
                "Conditions",
                Some(o.conditions.len()),
                v_flex()
                    .gap_1()
                    .children(o.conditions.iter().enumerate().map(|(i, c)| {
                        let color = match c.status.as_str() {
                            "True" => theme.success,
                            "False" => theme.danger,
                            _ => theme.muted_foreground,
                        };
                        let age = timestamp_age(&c.last_transition, now);
                        v_flex()
                            .id(("condition", i))
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .text_sm()
                                    .child(div().size_1p5().flex_none().rounded_full().bg(color))
                                    .child(div().font_semibold().child(c.type_.clone()))
                                    .child(div().text_color(color).child(c.status.clone()))
                                    .when(!c.reason.is_empty(), |el| {
                                        el.child(
                                            div()
                                                .text_color(theme.muted_foreground)
                                                .child(c.reason.clone()),
                                        )
                                    })
                                    .child(div().flex_1())
                                    .when_some(age, |el, age| {
                                        el.child(
                                            div()
                                                .text_xs()
                                                .font_features(tabular_figures())
                                                .text_color(theme.muted_foreground)
                                                .child(age),
                                        )
                                    }),
                            )
                            .when(!c.message.is_empty(), |el| {
                                el.child(
                                    div()
                                        .pl_3p5()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(c.message.clone()),
                                )
                            })
                    })),
                theme,
            ));
        }

        // Related events, newest first.
        let events = v_flex()
            .gap_1()
            .when_some(detail.events_error.as_ref(), |el, error| {
                el.child(
                    div()
                        .id("events-error")
                        .text_xs()
                        .text_color(theme.warning)
                        .child(format!("Events could not be read: {error}")),
                )
            })
            .when(
                detail.events.is_empty() && detail.events_error.is_none(),
                |el| {
                    el.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("No recent events."),
                    )
                },
            )
            .children(detail.events.iter().enumerate().map(|(i, e)| {
                let warning = e.type_.as_deref() == Some("Warning");
                let age = event_time(e)
                    .map(|t| human_duration(now - t.as_second()))
                    .unwrap_or_default();
                let count = e.count.filter(|c| *c > 1).map(|c| format!("×{c}"));
                v_flex()
                    .id(("event", i))
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(
                                div()
                                    .text_color(if warning {
                                        theme.warning
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(e.type_.clone().unwrap_or_default()),
                            )
                            .child(
                                div()
                                    .font_semibold()
                                    .child(e.reason.clone().unwrap_or_default()),
                            )
                            .children(count.map(|c| {
                                div().text_xs().text_color(theme.muted_foreground).child(c)
                            }))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .font_features(tabular_figures())
                                    .text_color(theme.muted_foreground)
                                    .child(age),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(e.message.clone().unwrap_or_default()),
                    )
            }))
            .when(detail.events_truncated, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Older events not shown."),
                )
            });
        body = body.child(section("Events", Some(detail.events.len()), events, theme));

        div()
            .id("detail-overview")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(body)
    }

    fn render_pairs(
        &self,
        title: &'static str,
        pairs: &[(String, String)],
        open: bool,
        toggle: Box<dyn Action>,
        theme: &Theme,
    ) -> impl IntoElement {
        let header = h_flex()
            .id(SharedString::from(format!("toggle-{title}")))
            .gap_1()
            .cursor_pointer()
            .text_xs()
            .font_semibold()
            .text_color(theme.muted_foreground)
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall(),
            )
            .child(format!("{} ({})", title.to_uppercase(), pairs.len()))
            .on_click(move |_, window, cx| {
                if let Err(e) = registry::perform(&*toggle, window, cx) {
                    tracing::warn!("{e}");
                }
            });
        v_flex()
            .gap_1()
            .child(header)
            .when(open && pairs.is_empty(), |el| {
                el.child(
                    div()
                        .pl_4()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("None"),
                )
            })
            .when(open, |el| {
                el.children(pairs.iter().enumerate().map(|(i, (k, v))| {
                    let full = format!("{k}: {v}");
                    h_flex()
                        .id(SharedString::from(format!("{title}-{i}")))
                        .pl_4()
                        .gap_2()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .child(
                            div()
                                .flex_none()
                                .max_w(relative(0.5))
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(k.clone()),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(v.clone()))
                        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
                }))
            })
    }

    fn render_secret(
        &self,
        secret: &object::SecretData,
        theme: &Theme,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = secret.entries.iter().enumerate().map(|(i, entry)| {
            let revealed = self.revealed.contains(&entry.key);
            let size = entry
                .value
                .len()
                .map(|n| format!("{n} bytes"))
                .unwrap_or_else(|| "not base64".to_owned());
            let value: SharedString = match (&entry.value, revealed) {
                (_, false) => MASK.into(),
                (SecretValue::Text(text), true) => text.clone().into(),
                (SecretValue::Binary(n), true) => format!("<binary, {n} bytes>").into(),
                (SecretValue::Invalid, true) => "<not valid base64>".into(),
            };
            let key = entry.key.clone();
            h_flex()
                .id(("secret", i))
                .gap_2()
                .items_start()
                .text_sm()
                .child(
                    div()
                        .w(px(160.))
                        .flex_none()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(entry.key.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("secret-value-{}", entry.key)))
                        .flex_1()
                        .min_w_0()
                        .font_family(theme.mono_font_family.clone())
                        .when(!revealed, |el| el.text_color(theme.muted_foreground))
                        .child(value),
                )
                .child(
                    div()
                        .text_xs()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(size),
                )
                .child(
                    Button::new(SharedString::from(format!("reveal-{}", entry.key)))
                        .icon(if revealed {
                            IconName::EyeOff
                        } else {
                            IconName::Eye
                        })
                        .xsmall()
                        .ghost()
                        .tooltip(if revealed {
                            "Hide value"
                        } else {
                            "Reveal value"
                        })
                        .on_click(registry::on_click(ToggleSecretValue { key })),
                )
        });
        section(
            "Data",
            Some(secret.entries.len()),
            v_flex()
                .gap_1()
                .when(!secret.type_.is_empty(), |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("Type: {}", secret.type_)),
                    )
                })
                .children(rows),
            theme,
        )
    }
}

/// A titled block of the Overview.
fn section(
    title: &str,
    count: Option<usize>,
    content: impl IntoElement,
    theme: &Theme,
) -> impl IntoElement {
    let title = match count {
        Some(n) => format!("{} ({n})", title.to_uppercase()),
        None => title.to_uppercase(),
    };
    v_flex()
        .gap_1p5()
        .child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(theme.muted_foreground)
                .child(title),
        )
        .child(content)
}

/// One metadata line; `mono` for names and identifiers.
fn key_value(
    id: (&'static str, usize),
    key: &str,
    value: String,
    mono: bool,
    theme: &Theme,
) -> AnyElement {
    let full = value.clone();
    h_flex()
        .id(id)
        .h(px(22.))
        .gap_2()
        .text_sm()
        .child(
            div()
                .w(px(120.))
                .flex_none()
                .text_color(theme.muted_foreground)
                .child(key.to_owned()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_features(tabular_figures())
                .when(mono, |el| el.font_family(theme.mono_font_family.clone()))
                .child(value),
        )
        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
        .into_any_element()
}

/// "5m" for an RFC 3339 timestamp five minutes ago.
fn timestamp_age(text: &str, now: i64) -> Option<String> {
    let seconds = text
        .parse::<k8s_openapi::jiff::Timestamp>()
        .ok()?
        .as_second();
    Some(human_duration(now - seconds))
}

impl Render for DetailView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let cx: &mut Context<Self> = cx;
        let content: AnyElement = match (&self.fetch, self.tab) {
            (Fetch::Loading, _) => h_flex()
                .flex_1()
                .justify_center()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(Spinner::new().small())
                .child("Loading…")
                .into_any_element(),
            (Fetch::Failed(error), _) => v_flex()
                .flex_1()
                .p_4()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.danger)
                        .child("Could not fetch this object"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(error.clone()),
                )
                .into_any_element(),
            (Fetch::Loaded { detail, overview }, DetailTab::Overview) => {
                // Arc clones: the render borrows the shared data, it does not copy it.
                let (detail, overview) = (detail.clone(), overview.clone());
                self.render_overview(&detail, &overview, &theme, cx)
                    .into_any_element()
            }
            (Fetch::Loaded { detail, .. }, DetailTab::Yaml) => v_flex()
                .id("detail-yaml")
                .flex_1()
                .min_h_0()
                .child(self.render_yaml_bar(detail, &theme, cx))
                .child(
                    div().flex_1().min_h_0().child(
                        Editor::new(&self.yaml)
                            .readonly(true)
                            .bordered(false)
                            .h_full(),
                    ),
                )
                .into_any_element(),
        };
        v_flex()
            .id("detail")
            .map(|el| Self::register_actions(el, cx))
            .test_support()
            .size_full()
            .min_w_0()
            .bg(theme.background)
            .child(self.render_header(&theme, cx))
            .children(self.render_notices(&theme))
            .child(content)
    }
}

#[path = "detail_actions.rs"]
mod actions;

#[cfg(any(test, feature = "automation"))]
#[path = "detail_state.rs"]
mod state;

#[cfg(test)]
mod tests {
    use super::timestamp_age;

    #[test]
    fn ages_of_rfc3339_timestamps() {
        assert_eq!(
            timestamp_age("2026-01-01T00:00:00Z", 1_767_225_900).as_deref(),
            Some("5m")
        );
        assert_eq!(timestamp_age("not a time", 0), None);
    }
}
