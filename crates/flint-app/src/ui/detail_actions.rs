//! The detail pane's registered actions ([`crate::ui::registry`]), their handlers, and the
//! checks the workspace's guards share with them. The pane's buttons, tabs and toggles dispatch
//! these actions.

use flint_core::{LogSubject, LogTarget};
use gpui_kit::*;

use super::super::log_view::LogRequest;
use super::super::object::PodContainer;
use super::super::registry::{
    ActionError, Bind, Close, OnView, OpenContainerLogs, OpenLogs, OpenPreviousLogs, Refresh,
    ShowOverview, ShowYaml, ToggleAnnotations, ToggleLabels, ToggleManagedFields,
    ToggleSecretValue,
};
use super::{DetailEvent, DetailTab, DetailView, Fetch};

impl DetailView {
    /// Every detail action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<Refresh>(|view, _, window, cx| view.refresh(window, cx))
            .bind::<Close>(|_, _, _, cx| cx.emit(DetailEvent::Close))
            .bind::<ShowOverview>(|view, _, _, cx| view.show_tab(DetailTab::Overview, cx))
            .bind::<ShowYaml>(|view, _, _, cx| view.show_tab(DetailTab::Yaml, cx))
            .bind::<ToggleManagedFields>(|view, _, window, cx| {
                if view.has_managed_fields() {
                    view.toggle_managed_fields(window, cx)
                }
            })
            .bind::<ToggleLabels>(|view, _, _, cx| {
                view.show_labels = !view.show_labels;
                cx.notify();
            })
            .bind::<ToggleAnnotations>(|view, _, _, cx| {
                view.show_annotations = !view.show_annotations;
                cx.notify();
            })
            .bind::<ToggleSecretValue>(Self::toggle_secret_value)
            .bind::<OpenLogs>(|view, _, _, cx| view.open_logs::<OpenLogs>(None, false, cx))
            .bind::<OpenPreviousLogs>(|view, _, _, cx| {
                view.open_logs::<OpenPreviousLogs>(None, true, cx)
            })
            .bind::<OpenContainerLogs>(|view, action, _, cx| {
                view.open_logs::<OpenContainerLogs>(Some(&action.container), action.previous, cx)
            })
    }

    /// The fetched pod's containers; empty for other kinds or before the fetch.
    pub(in crate::ui) fn log_containers(&self) -> &[PodContainer] {
        match &self.fetch {
            Fetch::Loaded { overview, .. } => &overview.containers,
            _ => &[],
        }
    }

    /// The log tab [`OpenLogs`], [`OpenPreviousLogs`] and [`OpenContainerLogs`] open: the named
    /// container (or the default one), and its previous instance only if it ran before.
    pub(in crate::ui) fn log_request<A: Action>(
        &self,
        container: Option<&str>,
        previous: bool,
    ) -> Result<LogRequest, ActionError> {
        let containers = self.log_containers();
        if containers.is_empty() {
            return Err(ActionError::rejected::<A>(
                "the detail pane shows no pod (or it is still loading)",
            ));
        }
        let found = match container {
            Some(name) => containers.iter().find(|c| c.name == name),
            None => containers.iter().find(|c| c.default),
        };
        let Some(found) = found else {
            return Err(ActionError::rejected::<A>(match container {
                Some(name) => format!("the pod has no container {name}"),
                None => "the pod has no app container".to_owned(),
            }));
        };
        if previous && !found.has_previous {
            return Err(ActionError::rejected::<A>(format!(
                "container {} has not restarted, so it has no previous logs",
                found.name
            )));
        }
        Ok(LogRequest {
            cluster: self.object.cluster.clone(),
            subject: LogSubject::Pod(LogTarget {
                namespace: self.object.namespace.clone().unwrap_or_default(),
                pod: self.object.name.clone(),
                container: Some(found.name.clone()),
            }),
            previous,
        })
    }

    pub(super) fn open_logs<A: Action>(
        &mut self,
        container: Option<&str>,
        previous: bool,
        cx: &mut Context<Self>,
    ) {
        match self.log_request::<A>(container, previous) {
            Ok(request) => cx.emit(DetailEvent::OpenLogs(request)),
            Err(e) => tracing::warn!("{e}"),
        }
    }

    pub(super) fn register_actions<E: InteractiveElement>(element: E, cx: &Context<Self>) -> E {
        Self::bind_actions(OnView::new(element, cx)).into_element()
    }

    fn show_tab(&mut self, tab: DetailTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            cx.notify();
        }
    }

    /// [`ToggleManagedFields`] needs a fetched object that has managedFields.
    pub(in crate::ui) fn has_managed_fields(&self) -> bool {
        matches!(&self.fetch, Fetch::Loaded { detail, .. } if detail.managed_fields.is_some())
    }

    /// [`ToggleSecretValue`] applies to Secrets only.
    pub(in crate::ui) fn is_secret(&self) -> bool {
        self.is_secret
    }

    /// [`ToggleSecretValue`] takes a key of the fetched Secret's data.
    pub(in crate::ui) fn check_secret_key(
        &self,
        action: &ToggleSecretValue,
    ) -> Result<(), ActionError> {
        let known = match &self.fetch {
            Fetch::Loaded { overview, .. } => overview
                .secret
                .as_ref()
                .is_some_and(|s| s.entries.iter().any(|e| e.key == action.key)),
            _ => false,
        };
        if known {
            Ok(())
        } else {
            Err(ActionError::rejected::<ToggleSecretValue>(format!(
                "the Secret has no key {}",
                action.key
            )))
        }
    }

    fn toggle_secret_value(
        &mut self,
        action: &ToggleSecretValue,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_secret || self.check_secret_key(action).is_err() {
            return;
        }
        if !self.revealed.remove(&action.key) {
            self.revealed.insert(action.key.clone());
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    #[test]
    fn every_detail_action_is_bound() {
        assert_binds_scope(Scope::Detail, super::DetailView::bind_actions);
    }
}
