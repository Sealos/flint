//! Keeps the navigation and the set's list header in step with what the members serve.

use flint_core::handle::{AdmittedMember, MemberStatus};
use flint_core::{ConnectionState, CoreHandle, KindId, Subscription as KindSubscription};
use gpui_kit::*;

use super::super::super::set_kinds::MemberKinds;
use super::super::super::set_members::MemberSignIn;
use super::Workspace;

impl Workspace {
    /// Gives the navigation the union of the open set's kinds, or takes it back once no set is
    /// open, and names the members without the open kind in the list header. A member that
    /// can't be acted on now (paused, moved, signed out) adds nothing and is never called
    /// lacking. Reads only what the core has already published.
    pub(in super::super) fn refresh_set_kinds(&mut self, cx: &mut Context<Self>) {
        let Some(run) = &self.sets.running else {
            self.nav
                .update(cx, |nav, cx| nav.set_cluster_set(None, None, cx));
            return;
        };
        let members = self.member_kinds(&run.members, cx);
        let (kind, list) = (run.kind.clone(), run.list.clone());
        let ready: Vec<AdmittedMember> = run
            .members
            .iter()
            .zip(&members)
            .filter(|(m, found)| run.held.contains(m.context()) && found.vouches_for(&kind))
            .map(|(m, _)| m.clone())
            .collect();
        let signed_out = self.signed_out_members(run, cx);
        let sign_in = MemberSignIn::new(cx.entity().downgrade());
        let note = self.nav.update(cx, |nav, cx| {
            nav.set_cluster_set(Some(members), Some(kind.clone()), cx);
            nav.set_kinds().and_then(|set| set.note(&kind))
        });
        list.update(cx, |list, cx| {
            list.set_signed_out(signed_out, sign_in, cx);
            list.set_kind_note(note, cx)
        });
        self.release_held(ready, &kind, cx);
    }

    /// What each of `members` can be said to serve now. Only a connected member's newest
    /// discovery is evidence; a member that can't be acted on contributes nothing.
    pub(super) fn member_kinds(&self, members: &[AdmittedMember], cx: &App) -> Vec<MemberKinds> {
        let names = self.cluster_names(cx);
        let model = self.model.read(cx);
        let reports = self.sets.running.as_ref().map_or(&[][..], |r| &r.reports);
        members
            .iter()
            .map(|member| {
                let cluster = member.context();
                let found = self
                    .set_row_refusal(cluster, cx)
                    .is_none()
                    .then(|| model.discovered(cluster))
                    .flatten();
                let connected = self
                    .cluster_summary(cluster, cx)
                    .is_some_and(|c| matches!(c.state, ConnectionState::Connected(_)));
                let signed_out = reports.iter().any(|r| {
                    r.cluster == cluster && matches!(r.status, MemberStatus::SignInNeeded { .. })
                });
                let down = signed_out
                    || reports.iter().any(|r| {
                        r.cluster == cluster && matches!(r.status, MemberStatus::Failed { .. })
                    });
                MemberKinds {
                    cluster: cluster.to_owned(),
                    shown: names.exact(cluster),
                    discovery: found.and_then(|d| d.discovery.clone()),
                    complete: connected && !down && found.is_some_and(|d| d.error.is_none()),
                    signed_out,
                }
            })
            .collect()
    }

    /// Subscribes the held members whose discovery now vouches for `kind`.
    fn release_held(&mut self, ready: Vec<AdmittedMember>, kind: &KindId, cx: &mut Context<Self>) {
        if ready.is_empty() {
            return;
        }
        let core = self.model.read(cx).core().clone();
        let Some(window) = self.sets.running.as_ref().map(|run| run.window) else {
            return;
        };
        for member in ready {
            // Bound: the core starts no LIST or watch for a member that has changed.
            let subscription = core
                .bound_to(member.clone())
                .subscribe(member.context(), kind);
            let cluster = member.context().to_owned();
            let feed = Self::member_feed(core.clone(), subscription, member, window, cx);
            if let Some(run) = &mut self.sets.running {
                run.held.remove(&cluster);
                run._feeds.push(feed);
            }
        }
    }

    /// Carries one member's snapshots into the combined list until it leaves its server.
    pub(super) fn member_feed(
        core: CoreHandle,
        mut subscription: KindSubscription,
        member: AdmittedMember,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            while subscription.changed().await {
                let snapshot = subscription.latest();
                // After the read: rows of a replacement server can only exist once the core
                // has recorded the move, so this check sees it.
                if let Some(why) = core.check_member(&member).await {
                    let _ = this.update(cx, |ws, cx| {
                        ws.discard_member(member.context(), why, cx);
                        ws.check_set_row(cx);
                    });
                    break;
                }
                let alive = window.update(cx, |_, window, cx| {
                    this.update(cx, |ws, cx| ws.feed_member(snapshot, window, cx))
                });
                if !matches!(alive, Ok(Ok(()))) {
                    break;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flint_core::{
        ConnectionState, Event, KindId, SafeErrorText, SectionKind, ServerVersion, SyncState,
    };
    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestAppContextExt as _;
    use serde_json::{Value, json};

    use crate::ui::set_kinds::{KindCoverage, MemberKinds};
    use crate::ui::support_tests::{
        CURRENT_GENERATION, TestWorkspace, WAIT, discovery, entry, open_workspace, section,
        standard_sections,
    };

    fn sprockets() -> KindId {
        KindId::new("acme.test", "sprockets")
    }

    fn connection(
        cx: &mut TestAppContext,
        t: &TestWorkspace,
        cluster: &str,
        state: ConnectionState,
    ) {
        t.feed(
            cx,
            Event::ConnectionChanged {
                cluster: cluster.into(),
                state,
            },
        );
    }

    fn connected() -> ConnectionState {
        ConnectionState::Connected(ServerVersion {
            git_version: "v1.33.1".into(),
            platform: "linux/arm64".into(),
        })
    }

    fn sprockets_coverage(cx: &mut TestAppContext, t: &TestWorkspace) -> KindCoverage {
        t.workspace.read_with(cx, |ws, cx| {
            let nav = ws.nav.read(cx);
            let set = nav.set_kinds().expect("the set's kinds");
            set.coverage(&sprockets()).cloned().expect("in the union")
        })
    }

    fn member_state(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str) -> SyncState {
        t.workspace.read_with(cx, |ws, _| {
            let run = ws.sets.running.as_ref().unwrap();
            let at = run.snapshots.iter().position(|s| s.cluster == cluster);
            run.snapshots[at.unwrap()].state.clone()
        })
    }

    fn discovered(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str, with_sprockets: bool) {
        connection(cx, t, cluster, connected());
        let mut sections = standard_sections();
        if with_sprockets {
            sections.push(section(
                "acme.test",
                SectionKind::ApiGroup,
                vec![entry(
                    "acme.test",
                    "sprockets",
                    "Sprocket",
                    "Sprockets",
                    true,
                )],
            ));
        }
        t.feed(
            cx,
            Event::DiscoveryChanged {
                cluster: cluster.into(),
                generation: CURRENT_GENERATION,
                discovery: Some(Arc::new(discovery(sections))),
                error: None,
            },
        );
    }

    async fn open_set(cx: &mut TestAppContext, t: &TestWorkspace) {
        t.dispatch(cx, "cluster_sets::ToggleClusterPicking", Value::Null)
            .unwrap();
        for cluster in ["kind-alpha", "kind-beta"] {
            t.dispatch(
                cx,
                "cluster_sets::TogglePickedCluster",
                json!({ "cluster": cluster }),
            )
            .unwrap();
        }
        t.dispatch(cx, "cluster_sets::OpenPickedClusters", Value::Null)
            .unwrap();
        t.wait_for_element(cx, "cluster-set-view").await;
    }

    fn note(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<String> {
        t.workspace.read_with(cx, |ws, cx| {
            let run = ws.sets.running.as_ref().unwrap();
            run.list.read(cx).kind_note().map(str::to_owned)
        })
    }

    #[gpui_kit::test]
    async fn a_kind_only_another_member_serves_opens_and_names_who_lacks_it(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        open_set(cx, &t).await;
        discovered(cx, &t, "kind-alpha", false);
        discovered(cx, &t, "kind-beta", true);
        cx.run_until_parked();

        let badge = t.workspace.read_with(cx, |ws, cx| {
            let nav = ws.nav.read(cx);
            let set = nav.set_kinds().expect("the set's kinds");
            set.coverage(&sprockets()).and_then(|c| c.badge())
        });
        assert_eq!(badge.as_deref(), Some("1/2"));

        t.dispatch(
            cx,
            "workspace::OpenView",
            json!({ "kind": sprockets().to_string() }),
        )
        .unwrap();
        cx.run_until_parked();
        let open = t
            .workspace
            .read_with(cx, |ws, _| ws.sets.running.as_ref().unwrap().kind.clone());
        assert_eq!(open, sprockets());
        let shown = note(cx, &t).expect("the header names the member");
        assert!(shown.starts_with("Sprockets not available on "), "{shown}");
        let badge_id = gpui_kit::SharedString::from(format!("nav-coverage-{}", sprockets()));
        assert!(t.find(cx, badge_id).is_some(), "the tree shows the badge");
        assert!(shown.contains("alpha"), "{shown}");
        assert!(!shown.contains("beta"), "{shown}");
    }

    #[gpui_kit::test]
    async fn a_member_still_discovering_is_not_called_lacking(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        open_set(cx, &t).await;
        discovered(cx, &t, "kind-beta", true);
        cx.run_until_parked();
        t.dispatch(
            cx,
            "workspace::OpenView",
            json!({ "kind": sprockets().to_string() }),
        )
        .unwrap();
        cx.run_until_parked();
        let shown = note(cx, &t).expect("the header says what is not known");
        assert!(shown.contains("not confirmed on"), "{shown}");
        assert!(!shown.contains("not available"), "{shown}");
    }

    #[gpui_kit::test]
    async fn a_member_whose_connection_failed_counts_neither_way(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        open_set(cx, &t).await;
        discovered(cx, &t, "kind-alpha", false);
        discovered(cx, &t, "kind-beta", true);
        cx.run_until_parked();
        assert_eq!(sprockets_coverage(cx, &t).lacking.len(), 1);

        let failed = ConnectionState::Failed(SafeErrorText::reduced("connection refused"));
        connection(cx, &t, "kind-alpha", failed);
        cx.run_until_parked();
        let coverage = sprockets_coverage(cx, &t);
        assert!(coverage.lacking.is_empty(), "{coverage:?}");
        assert_eq!(coverage.unconfirmed.len(), 1, "{coverage:?}");
    }

    #[gpui_kit::test]
    async fn a_failed_refresh_starts_no_list_until_discovery_vouches_again(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        open_set(cx, &t).await;
        discovered(cx, &t, "kind-alpha", true);
        discovered(cx, &t, "kind-beta", true);
        let cached = t.model.read_with(cx, |m, _| {
            m.discovered("kind-alpha").unwrap().discovery.clone()
        });
        t.feed(
            cx,
            Event::DiscoveryChanged {
                cluster: "kind-alpha".into(),
                generation: CURRENT_GENERATION,
                discovery: cached,
                error: Some("timed out".into()),
            },
        );
        cx.run_until_parked();
        t.dispatch(
            cx,
            "workspace::OpenView",
            json!({ "kind": sprockets().to_string() }),
        )
        .unwrap();
        cx.run_until_parked();
        assert_eq!(member_state(cx, &t, "kind-alpha"), SyncState::Idle);
        let held = |ws: &super::Workspace| {
            let mut held: Vec<_> = ws
                .sets
                .running
                .as_ref()
                .unwrap()
                .held
                .iter()
                .cloned()
                .collect();
            held.sort();
            held
        };
        // Only the member whose discovery can't vouch for the kind waits.
        assert_eq!(t.workspace.read_with(cx, |ws, _| held(ws)), ["kind-alpha"]);

        discovered(cx, &t, "kind-alpha", true);
        cx.run_until_parked();
        assert!(t.workspace.read_with(cx, |ws, _| held(ws)).is_empty());
    }

    #[gpui_kit::test]
    async fn a_set_with_no_kinds_yet_keeps_the_active_clusters_out(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        open_set(cx, &t).await;
        // The active cluster's kinds, with a set that has reported none.
        discovered(cx, &t, "kind-alpha", true);
        t.activate(cx, "kind-alpha").await;
        cx.run_until_parked();
        let silent = |cluster: &str| MemberKinds {
            cluster: cluster.into(),
            shown: cluster.into(),
            discovery: None,
            complete: false,
            signed_out: false,
        };
        let nav = t.workspace.read_with(cx, |ws, _| ws.nav_view());
        nav.update(cx, |nav, cx| {
            nav.set_cluster_set(Some(vec![silent("a"), silent("b")]), None, cx);
        });
        let shown = nav.read_with(cx, |nav, _| nav.has_sections());
        assert!(!shown, "the outside cluster's kinds are not the set's");
        let opened = t.workspace.read_with(cx, |ws, cx| {
            ws.view_entry(&sprockets().to_string(), cx).is_some()
        });
        assert!(!opened);
    }

    #[gpui_kit::test]
    async fn an_object_action_stays_on_the_active_clusters_kinds(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        open_set(cx, &t).await;
        discovered(cx, &t, "kind-alpha", false);
        discovered(cx, &t, "kind-beta", true);
        t.activate(cx, "kind-alpha").await;
        cx.run_until_parked();
        let refused = t.dispatch(
            cx,
            "workspace::OpenObject",
            json!({ "kind": sprockets().to_string(), "name": "one" }),
        );
        assert!(refused.is_err(), "only another member serves it");
        let open = t
            .workspace
            .read_with(cx, |ws, _| ws.sets.running.as_ref().unwrap().kind.clone());
        assert_eq!(open, KindId::pods());
    }

    #[gpui_kit::test]
    async fn closing_the_set_gives_the_navigation_back(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        open_set(cx, &t).await;
        discovered(cx, &t, "kind-beta", true);
        cx.run_until_parked();
        t.dispatch(cx, "cluster_sets::CloseClusterSet", Value::Null)
            .unwrap();
        // An unsaved set asks before it closes.
        cx.wait_for(t.window, WAIT, |window, cx| {
            gpui_kit::component::WindowExt::has_active_dialog(window, cx)
        })
        .await;
        t.click(cx, "ok");
        cx.run_until_parked();
        let cleared = t
            .workspace
            .read_with(cx, |ws, cx| ws.nav.read(cx).set_kinds().is_none());
        assert!(cleared);
    }
}
