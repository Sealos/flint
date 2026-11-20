//! The AI scope pickers of the Expose to AI and Edit AI Scope dialogs (ADR-0013, UP-14): which
//! namespaces and kinds agents may read of a cluster. Nothing checked means everything. The
//! choices come from the cluster's discovery (kinds) and its namespace list, plus whatever the
//! scope already names, so a scope saved earlier can be read and changed even while the
//! cluster is not connected.
//!
//! Each checkbox performs a registered action (`ai_scope::…`) from the picker's own focus, so
//! automation and the view tests take the same path as a click.

use std::collections::BTreeSet;

use flint_core::KindInfo;
use flint_core::settings::AiScope;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::registry::{
    self, Bind, OnView, ScopeAllKinds, ScopeAllNamespaces, ToggleScopeKind, ToggleScopeNamespace,
};

/// A kind the pickers offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindChoice {
    /// The kind id the scope stores (`pods`, `deployments.apps`).
    pub id: String,
    /// What the picker shows: the Kind, and the group when it has one.
    pub label: String,
    /// Cluster-scoped kinds are hidden under a namespace scope unless checked.
    pub namespaced: bool,
}

impl KindChoice {
    fn of(kind: &KindInfo) -> Self {
        let label = if kind.group().is_empty() {
            kind.kind().to_owned()
        } else {
            format!("{} ({})", kind.kind(), kind.group())
        };
        KindChoice {
            id: kind.id().as_str().to_owned(),
            label,
            namespaced: kind.namespaced,
        }
    }
}

pub struct ScopePicker {
    namespaces: Vec<String>,
    kinds: Vec<KindChoice>,
    selected_namespaces: BTreeSet<String>,
    selected_kinds: BTreeSet<String>,
    focus: FocusHandle,
}

impl ScopePicker {
    /// Pickers offering `namespaces` and the listable `kinds`, with `scope` checked. What the
    /// scope names but the cluster does not offer (now) is offered too.
    pub fn new(
        namespaces: &[String],
        kinds: &[KindInfo],
        scope: &AiScope,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut offered: BTreeSet<String> = namespaces.iter().cloned().collect();
        offered.extend(scope.namespaces.iter().cloned());
        let mut choices: Vec<KindChoice> = kinds
            .iter()
            .filter(|k| k.is_listable())
            .map(KindChoice::of)
            .collect();
        for id in &scope.kinds {
            if !choices.iter().any(|c| &c.id == id) {
                choices.push(KindChoice {
                    id: id.clone(),
                    label: id.clone(),
                    namespaced: true,
                });
            }
        }
        choices.sort_by_key(|c| c.label.to_lowercase());
        ScopePicker {
            namespaces: offered.into_iter().collect(),
            kinds: choices,
            selected_namespaces: scope.namespaces.iter().cloned().collect(),
            selected_kinds: scope.kinds.iter().cloned().collect(),
            focus: cx.focus_handle(),
        }
    }

    /// The scope checked now.
    pub fn scope(&self) -> AiScope {
        AiScope::new(
            self.selected_namespaces.iter().cloned().collect(),
            self.selected_kinds.iter().cloned().collect(),
        )
    }

    /// The namespaces and kinds offered.
    #[cfg(test)]
    pub fn offered(&self) -> (&[String], &[KindChoice]) {
        (&self.namespaces, &self.kinds)
    }

    /// The pickers' actions and their handlers.
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<ScopeAllNamespaces>(|this, _, _, cx| {
            this.selected_namespaces.clear();
            cx.notify();
        })
        .bind::<ScopeAllKinds>(|this, _, _, cx| {
            this.selected_kinds.clear();
            cx.notify();
        })
        .bind::<ToggleScopeNamespace>(|this, action, _, cx| {
            toggle(&mut this.selected_namespaces, action.namespace.trim());
            cx.notify();
        })
        .bind::<ToggleScopeKind>(|this, action, _, cx| {
            toggle(&mut this.selected_kinds, &action.kind.trim().to_lowercase());
            cx.notify();
        })
    }

    fn section(title: &'static str, cx: &App) -> Div {
        div()
            .pt_2()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(title)
    }

    fn empty(id: &'static str, text: &'static str, cx: &App) -> AnyElement {
        div()
            .id(id)
            .aria_label(text)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text)
            .test_support()
            .into_any_element()
    }
}

/// Adds `name` to `set`, or takes it out if it is there. Empty names are ignored.
fn toggle(set: &mut BTreeSet<String>, name: &str) {
    if name.is_empty() {
        return;
    }
    if !set.remove(name) {
        set.insert(name.to_owned());
    }
}

/// What a scope lets agents read, in a sentence: for the sidebar's tooltip and the dialogs.
pub fn summary(scope: &AiScope) -> String {
    let namespaces = match scope.namespaces.as_slice() {
        [] => "all namespaces".to_owned(),
        names => format!("namespaces {}", names.join(", ")),
    };
    let kinds = match scope.kinds.as_slice() {
        [] if scope.namespaces.is_empty() => "all kinds".to_owned(),
        [] => "all namespaced kinds".to_owned(),
        names => format!("kinds {}", names.join(", ")),
    };
    format!("{namespaces}; {kinds}")
}

impl Render for ScopePicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        let theme = cx.theme().clone();
        let namespace_rows = self.namespaces.iter().map(|ns| {
            Checkbox::new(SharedString::from(format!("scope-ns-{ns}")))
                .label(ns.clone())
                .checked(self.selected_namespaces.contains(ns))
                .on_click(registry::on_click_in(
                    focus.clone(),
                    ToggleScopeNamespace {
                        namespace: ns.clone(),
                    },
                ))
        });
        let hide_cluster_wide = !self.selected_namespaces.is_empty();
        let kind_rows = self.kinds.iter().map(|kind| {
            let label = if kind.namespaced {
                kind.label.clone()
            } else {
                format!("{} · cluster-wide", kind.label)
            };
            Checkbox::new(SharedString::from(format!("scope-kind-{}", kind.id)))
                .label(label)
                .checked(self.selected_kinds.contains(&kind.id))
                .on_click(registry::on_click_in(
                    focus.clone(),
                    ToggleScopeKind {
                        kind: kind.id.clone(),
                    },
                ))
        });
        let namespaces = v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(Self::section("Namespaces agents may read", cx))
            .child(
                Checkbox::new("scope-ns-all")
                    .label("All namespaces")
                    .checked(self.selected_namespaces.is_empty())
                    .on_click(registry::on_click_in(focus.clone(), ScopeAllNamespaces)),
            )
            .child(
                v_flex()
                    .id("scope-ns-list")
                    .max_h(px(150.))
                    .overflow_y_scroll()
                    .gap_1()
                    .when(self.namespaces.is_empty(), |el| {
                        el.child(Self::empty(
                            "scope-ns-empty",
                            "Select the cluster in Flint to list its namespaces here.",
                            cx,
                        ))
                    })
                    .children(namespace_rows),
            );
        let kinds = v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(Self::section("Kinds agents may read", cx))
            .child(
                Checkbox::new("scope-kind-all")
                    .label("All kinds")
                    .checked(self.selected_kinds.is_empty())
                    .on_click(registry::on_click_in(focus.clone(), ScopeAllKinds)),
            )
            .child(
                v_flex()
                    .id("scope-kind-list")
                    .max_h(px(150.))
                    .overflow_y_scroll()
                    .gap_1()
                    .when(self.kinds.is_empty(), |el| {
                        el.child(Self::empty(
                            "scope-kind-empty",
                            "Connect to the cluster to list its kinds here.",
                            cx,
                        ))
                    })
                    .children(kind_rows),
            );
        let element = v_flex()
            .id("ai-scope")
            .track_focus(&self.focus)
            .pt_2()
            .gap_1()
            .child(
                h_flex()
                    .items_start()
                    .gap_3()
                    .child(namespaces)
                    .child(div().w_px().self_stretch().bg(theme.border))
                    .child(kinds),
            )
            .when(hide_cluster_wide, |el| {
                el.child(Self::empty(
                    "scope-cluster-wide-note",
                    "With namespaces chosen, cluster-wide kinds are hidden unless checked.",
                    cx,
                ))
            })
            .test_support();
        Self::bind_actions(OnView::new(element, cx)).into_element()
    }
}

#[cfg(test)]
#[path = "scope_picker_tests.rs"]
mod tests;
