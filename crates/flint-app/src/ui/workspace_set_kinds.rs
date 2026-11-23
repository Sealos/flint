//! Keeps the navigation and the set's list header in step with what the members serve.

use gpui_kit::*;

use super::super::super::set_kinds::MemberKinds;
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
        let names = self.cluster_names(cx);
        let model = self.model.read(cx);
        let members: Vec<MemberKinds> = run
            .members
            .iter()
            .map(|member| {
                let cluster = member.context();
                let found = self
                    .set_row_refusal(cluster, cx)
                    .is_none()
                    .then(|| model.discovered(cluster))
                    .flatten();
                MemberKinds {
                    cluster: cluster.to_owned(),
                    shown: names.exact(cluster),
                    discovery: found.and_then(|d| d.discovery.clone()),
                    complete: found.is_some_and(|d| d.error.is_none()),
                }
            })
            .collect();
        let (kind, list) = (run.kind.clone(), run.list.clone());
        let note = self.nav.update(cx, |nav, cx| {
            nav.set_cluster_set(Some(members), Some(kind.clone()), cx);
            nav.set_kinds().and_then(|set| set.note(&kind))
        });
        list.update(cx, |list, cx| list.set_kind_note(note, cx));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flint_core::{Event, KindId, SectionKind};
    use gpui_kit::TestAppContext;
    use serde_json::{Value, json};

    use crate::ui::support_tests::{
        CURRENT_GENERATION, TestWorkspace, discovery, entry, open_workspace, section,
        standard_sections,
    };

    fn sprockets() -> KindId {
        KindId::new("acme.test", "sprockets")
    }

    fn discovered(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str, with_sprockets: bool) {
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
    async fn closing_the_set_gives_the_navigation_back(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        open_set(cx, &t).await;
        discovered(cx, &t, "kind-beta", true);
        cx.run_until_parked();
        t.dispatch(cx, "cluster_sets::CloseClusterSet", Value::Null)
            .unwrap();
        cx.run_until_parked();
        let cleared = t
            .workspace
            .read_with(cx, |ws, cx| ws.nav.read(cx).set_kinds().is_none());
        assert!(cleared);
    }
}
