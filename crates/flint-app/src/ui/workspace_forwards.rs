//! The workspace's port-forward actions: Forward Port on the selected pod or service, the panel,
//! and its Start, Stop and Open in Browser. Each refuses, saying why, under the same checks as a
//! shell; the core's gate decides again ([`flint_core::port_forward`]).

use flint_core::ClusterId;
use gpui_kit::*;

use super::super::dock::{DockTab, TabKey};
use super::super::port_forward_view::{self, FormBinding, FormTarget, PortForwardsView};
use super::super::registry::{
    ForwardPort, OpenForwardedPort, ShowPortForwards, StartPortForward, StopAllPortForwards,
    StopPortForward,
};
use super::Workspace;

impl Workspace {
    /// Every forwards tab's view.
    fn forward_views(&self, cx: &App) -> Vec<Entity<PortForwardsView>> {
        self.dock.read(cx).forward_views().cloned().collect()
    }

    /// The view holding forward `id`.
    fn forward_view(&self, id: u64, cx: &App) -> Option<Entity<PortForwardsView>> {
        self.forward_views(cx)
            .into_iter()
            .find(|view| view.read(cx).has_row(id))
    }

    /// Stops every forward in this window.
    pub(super) fn stop_forwards(&mut self, cx: &mut Context<Self>) {
        for view in self.forward_views(cx) {
            view.update(cx, |view, cx| view.stop_all(cx));
        }
    }

    pub(super) fn any_forward_running(&self, cx: &App) -> bool {
        self.forward_views(cx)
            .iter()
            .any(|view| view.read(cx).has_running())
    }

    pub(super) fn has_forward(&self, id: u64, cx: &App) -> bool {
        self.forward_view(id, cx).is_some()
    }

    pub(super) fn forward_url(&self, id: u64, cx: &App) -> Option<String> {
        self.forward_view(id, cx)?.read(cx).url(id)
    }

    /// The forwards tab shown, when its form has a target.
    pub(super) fn can_start_forward(&self, cx: &App) -> bool {
        self.dock
            .read(cx)
            .active_forwards()
            .is_some_and(|view| view.read(cx).can_start())
    }

    /// Why `cluster` takes no new forward now, if so.
    fn forward_blocked(&self, cluster: &str, cx: &App) -> Option<String> {
        if let Some(reason) = self.sessions_blocked(cluster, cx) {
            return Some(reason);
        }
        if self.is_protected(cluster, cx) {
            let shown = self.cluster_names(cx).exact(cluster);
            return Some(super::protected_reason(&shown));
        }
        if let Err(why) = self.session_member(cluster, cx) {
            return Some(why);
        }
        self.writes_paused_why(cluster, cx)
    }

    /// Opens (or shows) `cluster`'s forwards tab.
    fn open_forwards(
        &mut self,
        cluster: ClusterId,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<PortForwardsView>> {
        let member = self.session_member(&cluster, cx).ok()?;
        let key = TabKey::Forwards(cluster.clone());
        let tab_cluster = self.tab_cluster(&key, cx);
        let id = self.open_tab(
            key,
            tab_cluster,
            move |window, cx| {
                DockTab::Forwards(cx.new(|cx| PortForwardsView::new(cluster, window, cx)))
            },
            focus,
            window,
            cx,
        );
        self.sets.keep_tab_pin(id, member);
        match self.dock.read(cx).tab(id) {
            Some(DockTab::Forwards(view)) => Some(view.clone()),
            _ => None,
        }
    }

    /// Forward Port: the panel, its form on the selected pod or service.
    pub(super) fn forward_port(
        &mut self,
        _: &ForwardPort,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.detail_view() else {
            return;
        };
        let (cluster, target, uid, checked) = {
            let detail = detail.read(cx);
            (
                detail.object().cluster.clone(),
                detail.forward_target(),
                detail.forward_uid(),
                detail.forward_check::<ForwardPort>(),
            )
        };
        if let Err(e) = checked {
            let why = super::super::detail::reason(&e);
            self.refuse(port_forward_view::refused(&why), cx);
            return;
        }
        if let Some(why) = self.forward_blocked(&cluster, cx) {
            self.refuse(port_forward_view::refused(&why), cx);
            return;
        }
        let Some((namespace, target, ports)) = target else {
            return;
        };
        let member = match self.session_member(&cluster, cx) {
            Ok(member) => member,
            Err(why) => {
                self.refuse(port_forward_view::refused(&why), cx);
                return;
            }
        };
        let binding = FormBinding {
            core: self.session_core(member.as_ref(), cx),
            server: match &member {
                Some(member) => Some(member.server().to_owned()),
                None => self.shown_server(&cluster, cx),
            },
            member,
        };
        if let Some(view) = self.open_forwards(cluster, true, window, cx) {
            let form = FormTarget {
                namespace,
                target,
                uid,
                ports,
            };
            view.update(cx, |view, cx| view.set_target(form, binding, window, cx));
        }
    }

    /// Show Port Forwards: the active cluster's panel.
    pub(super) fn show_port_forwards(
        &mut self,
        _: &ShowPortForwards,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(cluster) = self.cluster.clone() {
            self.open_forwards(cluster, true, window, cx);
        }
    }

    pub(super) fn start_port_forward(
        &mut self,
        _: &StartPortForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.dock.read(cx).active_forwards().cloned() else {
            return;
        };
        let cluster = view.read(cx).tab_cluster().clone();
        if let Some(why) = self.forward_blocked(&cluster, cx) {
            self.refuse(port_forward_view::refused(&why), cx);
            return;
        }
        // The set around this cluster changed since its target was selected.
        let member = self.session_member(&cluster, cx).ok().flatten();
        if !view.read(cx).form_bound_to(member.as_ref()) {
            view.update(cx, |view, cx| view.clear_form(cx));
            let why = "its cluster's connection changed; select the target again";
            self.refuse(port_forward_view::refused(why), cx);
            return;
        }
        view.update(cx, |view, cx| view.start(cx));
    }

    pub(super) fn stop_port_forward(
        &mut self,
        action: &StopPortForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.forward_view(action.id, cx) {
            view.update(cx, |view, cx| view.stop(action.id, cx));
        }
    }

    pub(super) fn stop_all_port_forwards(
        &mut self,
        _: &StopAllPortForwards,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_forwards(cx);
    }

    pub(super) fn open_forwarded_port(
        &mut self,
        action: &OpenForwardedPort,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(url) = self.forward_url(action.id, cx) {
            cx.open_url(&url);
        }
    }
}

#[cfg(test)]
#[path = "workspace_forwards_tests.rs"]
mod tests;
