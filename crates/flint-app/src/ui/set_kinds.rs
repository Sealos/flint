//! Which kinds an open cluster set serves, and on which members.
//!
//! The navigation shows the union of what the members discovered. A member is only said to lack
//! a kind when its discovery can prove it: a member with no usable discovery, a failed refresh or
//! an incomplete API group is "not confirmed" instead. Everything here reads discovery the core
//! already holds, so it sends nothing to any cluster.

use std::collections::HashMap;
use std::sync::Arc;

use flint_core::{ClusterDiscovery, ClusterId, KindId, KindInfo, NavEntry};

/// What one member contributes.
#[derive(Debug, Clone)]
pub struct MemberKinds {
    pub cluster: ClusterId,
    /// The member's name on screen.
    pub shown: String,
    /// `None` while nothing can be said of it: not discovered yet, paused, or no longer the
    /// server it was admitted on.
    pub discovery: Option<Arc<ClusterDiscovery>>,
    /// Whether the newest discovery succeeded. If not, `discovery` is an older result, whose
    /// missing kinds prove nothing.
    pub complete: bool,
}

impl MemberKinds {
    fn same(&self, other: &Self) -> bool {
        let same_discovery = match (&self.discovery, &other.discovery) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        same_discovery
            && self.complete == other.complete
            && self.cluster == other.cluster
            && self.shown == other.shown
    }

    fn presence(&self, kind: &KindId) -> Presence {
        let Some(discovery) = &self.discovery else {
            return Presence::Unknown;
        };
        // A failed refresh or a skipped group leaves older kinds in place: they prove nothing
        // either way, so a cached entry is no more evidence than a missing one.
        let current = self.complete
            && !discovery
                .incomplete_groups
                .iter()
                .any(|g| g == kind.group());
        if !current {
            Presence::Unknown
        } else if discovery.kind(kind).is_some_and(KindInfo::is_listable) {
            Presence::Serves
        } else {
            Presence::Lacks
        }
    }

    /// Whether `kind` may be subscribed to on this member. A member with a discovery that can't
    /// vouch for the kind waits: the core would list from its cached entry, or call the kind
    /// unavailable. With no discovery the core starts nothing until one arrives.
    pub fn may_subscribe(&self, kind: &KindId) -> bool {
        self.discovery.is_none() || self.presence(kind) != Presence::Unknown
    }

    /// Whether this member was held back from `kind` and its discovery now vouches for it.
    pub fn vouches_for(&self, kind: &KindId) -> bool {
        self.discovery.is_some() && self.may_subscribe(kind)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Serves,
    Lacks,
    Unknown,
}

/// Where a kind can be listed in the set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KindCoverage {
    pub serving: usize,
    pub total: usize,
    /// Members proven not to offer a list of this kind, by name on screen.
    pub lacking: Vec<String>,
    /// Members that may or may not serve it, by name on screen.
    pub unconfirmed: Vec<String>,
}

impl KindCoverage {
    fn of(members: &[MemberKinds], kind: &KindId) -> Self {
        let mut coverage = KindCoverage {
            total: members.len(),
            ..Self::default()
        };
        for member in members {
            match member.presence(kind) {
                Presence::Serves => coverage.serving += 1,
                Presence::Lacks => coverage.lacking.push(member.shown.clone()),
                Presence::Unknown => coverage.unconfirmed.push(member.shown.clone()),
            }
        }
        coverage
    }

    /// The navigation's badge, such as `2/3`. `None` when every member serves the kind.
    pub fn badge(&self) -> Option<String> {
        (self.serving < self.total).then(|| format!("{}/{}", self.serving, self.total))
    }

    /// The navigation tooltip's lines about the set.
    pub fn detail(&self) -> String {
        let mut lines = vec![format!("on {} of {} clusters", self.serving, self.total)];
        if !self.lacking.is_empty() {
            lines.push(format!("not available on {}", self.lacking.join(", ")));
        }
        if !self.unconfirmed.is_empty() {
            lines.push(format!("not confirmed on {}", self.unconfirmed.join(", ")));
        }
        lines.join("\n")
    }

    /// The list header's sentence naming the members without this kind, if any.
    pub fn note(&self, label: &str) -> Option<String> {
        match (self.lacking.is_empty(), self.unconfirmed.is_empty()) {
            (true, true) => None,
            (false, true) => Some(format!(
                "{label} not available on {}.",
                self.lacking.join(", ")
            )),
            (true, false) => Some(format!(
                "{label} not confirmed on {}.",
                self.unconfirmed.join(", ")
            )),
            (false, false) => Some(format!(
                "{label} not available on {}. Not confirmed on {}.",
                self.lacking.join(", "),
                self.unconfirmed.join(", ")
            )),
        }
    }
}

/// The union of the members' kinds, with each kind's coverage.
#[derive(Debug)]
pub struct SetKinds {
    members: Vec<MemberKinds>,
    discovery: ClusterDiscovery,
    coverage: HashMap<KindId, KindCoverage>,
}

impl SetKinds {
    /// `members` must not be empty: an admitted set has at least two.
    pub fn new(members: Vec<MemberKinds>) -> Self {
        let mut kinds: Vec<KindInfo> = Vec::new();
        let mut at: HashMap<KindId, usize> = HashMap::new();
        for kind in members
            .iter()
            .filter_map(|m| m.discovery.as_ref())
            .flat_map(|d| &d.kinds)
        {
            match at.get(&kind.id()) {
                None => {
                    at.insert(kind.id(), kinds.len());
                    kinds.push(kind.clone());
                }
                // A member that can list the kind gives it its entry.
                Some(&i) if !kinds[i].is_listable() && kind.is_listable() => {
                    kinds[i] = kind.clone();
                }
                Some(_) => {}
            }
        }
        let coverage = kinds
            .iter()
            .map(|kind| (kind.id(), KindCoverage::of(&members, &kind.id())))
            .collect();
        SetKinds {
            members,
            discovery: ClusterDiscovery::new(kinds),
            coverage,
        }
    }

    /// Whether this was built from exactly `members`, so there is nothing to rebuild.
    pub fn built_from(&self, members: &[MemberKinds]) -> bool {
        self.members.len() == members.len()
            && self.members.iter().zip(members).all(|(a, b)| a.same(b))
    }

    /// The union, as one discovery: the navigation and the kind resolver work on it unchanged.
    pub fn discovery(&self) -> &ClusterDiscovery {
        &self.discovery
    }

    /// The entry for `kind`: its exact id, as the navigation sends it, else named as `kubectl
    /// get` takes it. The exact id comes first because one member's plural can be another's
    /// singular, which the name alone can't tell apart.
    pub fn entry(&self, kind: &str) -> Option<NavEntry> {
        let exact = self
            .discovery
            .navigation
            .sections
            .iter()
            .flat_map(|s| &s.entries)
            .find(|e| e.id.as_str() == kind);
        exact
            .or_else(|| self.discovery.nav_entry(kind).ok())
            .cloned()
    }

    pub fn has_kinds(&self) -> bool {
        !self.discovery.navigation.sections.is_empty()
    }

    /// The name the navigation keeps its open sections under while no cluster is active.
    pub fn first_member(&self) -> Option<&ClusterId> {
        self.members.first().map(|m| &m.cluster)
    }

    pub fn coverage(&self, kind: &KindId) -> Option<&KindCoverage> {
        self.coverage.get(kind)
    }

    /// The list header's note for `kind`, also when no member lists it any more.
    pub fn note(&self, kind: &KindId) -> Option<String> {
        let label = self
            .discovery
            .navigation
            .entry(kind)
            .map_or_else(|| kind.plural().to_owned(), |e| e.label.clone());
        match self.coverage(kind) {
            Some(coverage) => coverage.note(&label),
            None => KindCoverage::of(&self.members, kind).note(&label),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::support_tests::{discovery, entry, section};
    use flint_core::SectionKind;

    fn widgets() -> KindId {
        KindId::new("example.com", "widgets")
    }

    fn found(with_widgets: bool) -> ClusterDiscovery {
        let mut sections = vec![section(
            "Workloads",
            SectionKind::BuiltIn,
            vec![entry("", "pods", "Pod", "Pods", true)],
        )];
        if with_widgets {
            sections.push(section(
                "example.com",
                SectionKind::ApiGroup,
                vec![entry("example.com", "widgets", "Widget", "Widgets", true)],
            ));
        }
        let found = discovery(sections);
        ClusterDiscovery::new(found.kinds)
    }

    fn member(name: &str, found: ClusterDiscovery) -> MemberKinds {
        MemberKinds {
            cluster: name.into(),
            shown: name.to_uppercase(),
            discovery: Some(Arc::new(found)),
            complete: true,
        }
    }

    fn silent(name: &str) -> MemberKinds {
        MemberKinds {
            cluster: name.into(),
            shown: name.to_uppercase(),
            discovery: None,
            complete: false,
        }
    }

    #[test]
    fn a_kind_on_one_member_is_in_the_union_and_names_who_lacks_it() {
        let set = SetKinds::new(vec![
            member("a", found(false)),
            member("b", found(true)),
            member("c", found(true)),
        ]);
        assert!(set.discovery().navigation.entry(&widgets()).is_some());
        let coverage = set.coverage(&widgets()).unwrap();
        assert_eq!(coverage.badge().as_deref(), Some("2/3"));
        assert_eq!(coverage.lacking, ["A"]);
        assert_eq!(
            set.note(&widgets()).as_deref(),
            Some("Widgets not available on A.")
        );
        assert_eq!(set.coverage(&KindId::pods()).unwrap().badge(), None);
        assert_eq!(set.note(&KindId::pods()), None);
    }

    #[test]
    fn an_incomplete_group_is_not_reported_as_lacking() {
        let partial = found(false);
        let partial =
            ClusterDiscovery::with_incomplete_groups(partial.kinds, vec!["example.com".into()]);
        let set = SetKinds::new(vec![member("a", partial), member("b", found(true))]);
        let coverage = set.coverage(&widgets()).unwrap();
        assert!(coverage.lacking.is_empty());
        assert_eq!(coverage.unconfirmed, ["A"]);
        assert_eq!(coverage.badge().as_deref(), Some("1/2"));
        assert_eq!(
            set.note(&widgets()).as_deref(),
            Some("Widgets not confirmed on A.")
        );
    }

    #[test]
    fn a_failed_refresh_proves_nothing_missing_but_keeps_what_it_knew() {
        let mut stale = member("a", found(false));
        stale.complete = false;
        let set = SetKinds::new(vec![stale, member("b", found(true))]);
        let coverage = set.coverage(&widgets()).unwrap();
        assert!(coverage.lacking.is_empty());
        assert_eq!(coverage.unconfirmed, ["A"]);
        // What it knew stays in the navigation, but no longer counts as served.
        let pods = set.coverage(&KindId::pods()).unwrap();
        assert_eq!(pods.serving, 1);
        assert_eq!(pods.unconfirmed, ["A"]);
    }

    #[test]
    fn a_kind_kept_from_a_skipped_group_is_not_confirmed() {
        let kept = found(true);
        let kept = ClusterDiscovery::with_incomplete_groups(kept.kinds, vec!["example.com".into()]);
        let set = SetKinds::new(vec![member("a", kept), member("b", found(true))]);
        let coverage = set.coverage(&widgets()).unwrap();
        assert_eq!(coverage.serving, 1);
        assert_eq!(coverage.unconfirmed, ["A"]);
    }

    #[test]
    fn an_exact_kind_wins_over_another_members_singular() {
        let of = |plural: &str, kind: &str, label: &str| {
            let sections = vec![section(
                "example.com",
                SectionKind::ApiGroup,
                vec![entry("example.com", plural, kind, label, true)],
            )];
            ClusterDiscovery::new(discovery(sections).kinds)
        };
        let set = SetKinds::new(vec![
            member("a", of("foos", "Foo", "Foos")),
            member("b", of("fooses", "Foos", "Fooses")),
        ]);
        let foos = KindId::new("example.com", "foos");
        assert_eq!(set.entry(&foos.to_string()).map(|e| e.id), Some(foos));
    }

    #[test]
    fn a_member_without_discovery_adds_nothing_and_lacks_nothing() {
        let set = SetKinds::new(vec![silent("paused"), member("b", found(false))]);
        assert!(set.discovery().navigation.entry(&widgets()).is_none());
        let pods = set.coverage(&KindId::pods()).unwrap();
        assert!(pods.lacking.is_empty());
        assert_eq!(pods.unconfirmed, ["PAUSED"]);
    }

    #[test]
    fn a_kind_a_member_cannot_list_counts_against_it_but_keeps_the_entry() {
        let mut unlistable = found(true);
        for kind in &mut unlistable.kinds {
            if kind.id() == widgets() {
                kind.verbs.retain(|v| v != "list");
            }
        }
        let unlistable = ClusterDiscovery::new(unlistable.kinds);
        let set = SetKinds::new(vec![member("a", unlistable), member("b", found(true))]);
        assert!(set.discovery().navigation.entry(&widgets()).is_some());
        assert_eq!(set.coverage(&widgets()).unwrap().lacking, ["A"]);
    }

    #[test]
    fn the_note_names_both_kinds_of_gap() {
        let coverage = KindCoverage {
            serving: 1,
            total: 3,
            lacking: vec!["A".into()],
            unconfirmed: vec!["B".into()],
        };
        assert_eq!(
            coverage.note("Widgets").as_deref(),
            Some("Widgets not available on A. Not confirmed on B.")
        );
    }

    #[test]
    fn the_same_inputs_need_no_rebuild() {
        let members = vec![member("a", found(true)), silent("b")];
        let set = SetKinds::new(members.clone());
        assert!(set.built_from(&members));
        let mut renamed = members.clone();
        renamed[1].shown = "other".into();
        assert!(!set.built_from(&renamed));
        let mut refreshed = members;
        refreshed[0].discovery = Some(Arc::new(found(true)));
        assert!(!set.built_from(&refreshed));
    }
}
