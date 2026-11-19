//! A cluster's discovery results and the navigation built from them: built-in kinds in fixed
//! sections, then everything else (CRDs, aggregated APIs) grouped by API group.

use std::collections::BTreeMap;

use crate::read::{KindId, KindInfo, ResolveKindError};

/// Discovery results for one cluster, kept while the cluster is not active.
#[derive(Debug, Clone)]
pub struct ClusterDiscovery {
    /// Every kind at its group's preferred version, core kinds first.
    pub kinds: Vec<KindInfo>,
    pub navigation: Navigation,
}

impl ClusterDiscovery {
    pub fn new(kinds: Vec<KindInfo>) -> Self {
        let navigation = Navigation::build(&kinds);
        ClusterDiscovery { kinds, navigation }
    }

    pub fn kind(&self, id: &KindId) -> Option<&KindInfo> {
        self.kinds.iter().find(|k| k.id() == *id)
    }

    /// The kind a user typed, as `kubectl get` accepts it (`deploy`, `po`, `Deployment.apps`):
    /// see [`resolve_kind`](crate::read::resolve_kind).
    pub fn resolve(&self, query: &str) -> Result<&KindInfo, ResolveKindError> {
        crate::read::resolve_kind(&self.kinds, query, None)
    }

    /// The navigation entry for a kind the user typed (`deploy`, `po`, `Deployment.apps`), for
    /// opening its list. A kind can resolve and still have no entry: only listable kinds are
    /// in the navigation.
    pub fn nav_entry(&self, query: &str) -> Result<&NavEntry, NavEntryError> {
        let kind = self.resolve(query).map_err(NavEntryError::Unresolved)?;
        let id = kind.id();
        self.navigation
            .entry(&id)
            .ok_or(NavEntryError::NotInNavigation(id))
    }
}

/// Why [`ClusterDiscovery::nav_entry`] found no entry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NavEntryError {
    #[error(transparent)]
    Unresolved(ResolveKindError),
    /// The name resolved to this kind, which is served but cannot be listed, so has no view.
    #[error("{0} is served but cannot be listed, so it has no list to open")]
    NotInNavigation(KindId),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Navigation {
    pub sections: Vec<NavSection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    /// A fixed section of built-in kinds.
    BuiltIn,
    /// Built-in kinds not placed in a fixed section.
    Other,
    /// One API group of CRDs or an aggregated API, titled by the group name.
    ApiGroup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavSection {
    pub title: String,
    pub kind: SectionKind,
    pub entries: Vec<NavEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavEntry {
    pub id: KindId,
    /// What to show, the same way for every kind: spaced and plural ("Pods", "Config Maps",
    /// "Cluster Trust Bundles", "Widgets").
    pub label: String,
    pub kind: String,
    pub api_version: String,
    pub namespaced: bool,
    pub listable: bool,
    pub watchable: bool,
}

impl NavEntry {
    fn new(kind: &KindInfo, label: &str) -> Self {
        NavEntry {
            id: kind.id(),
            label: label.to_owned(),
            kind: kind.kind().to_owned(),
            api_version: kind.api_version().to_owned(),
            namespaced: kind.namespaced,
            listable: kind.is_listable(),
            watchable: kind.is_watchable(),
        }
    }
}

/// Fixed sections: (title, [(group, plural, label)]), in display order.
/// (group, plural, label).
type Entry = (&'static str, &'static str, &'static str);

const SECTIONS: &[(&str, &[Entry])] = &[
    (
        "Workloads",
        &[
            ("", "pods", "Pods"),
            ("apps", "deployments", "Deployments"),
            ("apps", "daemonsets", "Daemon Sets"),
            ("apps", "statefulsets", "Stateful Sets"),
            ("apps", "replicasets", "Replica Sets"),
            ("", "replicationcontrollers", "Replication Controllers"),
            ("batch", "jobs", "Jobs"),
            ("batch", "cronjobs", "Cron Jobs"),
        ],
    ),
    (
        "Network",
        &[
            ("", "services", "Services"),
            ("", "endpoints", "Endpoints"),
            ("discovery.k8s.io", "endpointslices", "Endpoint Slices"),
            ("networking.k8s.io", "ingresses", "Ingresses"),
            ("networking.k8s.io", "ingressclasses", "Ingress Classes"),
            ("networking.k8s.io", "networkpolicies", "Network Policies"),
        ],
    ),
    (
        "Config & Storage",
        &[
            ("", "configmaps", "Config Maps"),
            ("", "secrets", "Secrets"),
            ("", "persistentvolumeclaims", "Persistent Volume Claims"),
            ("", "persistentvolumes", "Persistent Volumes"),
            ("storage.k8s.io", "storageclasses", "Storage Classes"),
            ("", "resourcequotas", "Resource Quotas"),
            ("", "limitranges", "Limit Ranges"),
            (
                "autoscaling",
                "horizontalpodautoscalers",
                "Horizontal Pod Autoscalers",
            ),
            ("policy", "poddisruptionbudgets", "Pod Disruption Budgets"),
            ("scheduling.k8s.io", "priorityclasses", "Priority Classes"),
            ("node.k8s.io", "runtimeclasses", "Runtime Classes"),
            ("coordination.k8s.io", "leases", "Leases"),
            ("storage.k8s.io", "volumeattachments", "Volume Attachments"),
            ("storage.k8s.io", "csidrivers", "CSI Drivers"),
            ("storage.k8s.io", "csinodes", "CSI Nodes"),
            (
                "storage.k8s.io",
                "csistoragecapacities",
                "CSI Storage Capacities",
            ),
        ],
    ),
    (
        "Cluster",
        &[
            ("", "nodes", "Nodes"),
            ("", "namespaces", "Namespaces"),
            ("", "events", "Events"),
            (
                "apiextensions.k8s.io",
                "customresourcedefinitions",
                "Custom Resource Definitions",
            ),
            ("apiregistration.k8s.io", "apiservices", "API Services"),
            (
                "admissionregistration.k8s.io",
                "mutatingwebhookconfigurations",
                "Mutating Webhooks",
            ),
            (
                "admissionregistration.k8s.io",
                "validatingwebhookconfigurations",
                "Validating Webhooks",
            ),
            (
                "admissionregistration.k8s.io",
                "validatingadmissionpolicies",
                "Validating Admission Policies",
            ),
            (
                "admissionregistration.k8s.io",
                "validatingadmissionpolicybindings",
                "Validating Admission Policy Bindings",
            ),
            (
                "flowcontrol.apiserver.k8s.io",
                "flowschemas",
                "Flow Schemas",
            ),
            (
                "flowcontrol.apiserver.k8s.io",
                "prioritylevelconfigurations",
                "Priority Levels",
            ),
            (
                "certificates.k8s.io",
                "certificatesigningrequests",
                "Certificate Signing Requests",
            ),
        ],
    ),
    (
        "Access Control",
        &[
            ("", "serviceaccounts", "Service Accounts"),
            ("rbac.authorization.k8s.io", "roles", "Roles"),
            ("rbac.authorization.k8s.io", "rolebindings", "Role Bindings"),
            ("rbac.authorization.k8s.io", "clusterroles", "Cluster Roles"),
            (
                "rbac.authorization.k8s.io",
                "clusterrolebindings",
                "Cluster Role Bindings",
            ),
        ],
    ),
];

/// Kinds not shown because another entry shows the same objects.
const HIDDEN: &[(&str, &str)] = &[("events.k8s.io", "events")];

/// Groups served by the Kubernetes API server itself. Everything else (CRDs, including those
/// in `*.k8s.io` groups such as the Gateway API, and aggregated APIs such as metrics) is shown
/// by API group.
const BUILTIN_GROUPS: &[&str] = &[
    "",
    "admissionregistration.k8s.io",
    "apiextensions.k8s.io",
    "apiregistration.k8s.io",
    "apps",
    "authentication.k8s.io",
    "authorization.k8s.io",
    "autoscaling",
    "batch",
    "certificates.k8s.io",
    "coordination.k8s.io",
    "discovery.k8s.io",
    "events.k8s.io",
    "flowcontrol.apiserver.k8s.io",
    "internal.apiserver.k8s.io",
    "networking.k8s.io",
    "node.k8s.io",
    "policy",
    "rbac.authorization.k8s.io",
    "resource.k8s.io",
    "scheduling.k8s.io",
    "storage.k8s.io",
    "storagemigration.k8s.io",
];

fn is_builtin_group(group: &str) -> bool {
    BUILTIN_GROUPS.contains(&group)
}

impl Navigation {
    /// Only listable kinds appear.
    pub fn build(kinds: &[KindInfo]) -> Self {
        let listable: Vec<&KindInfo> = kinds
            .iter()
            .filter(|k| k.is_listable())
            .filter(|k| {
                !HIDDEN
                    .iter()
                    .any(|(g, p)| k.group() == *g && k.plural() == *p)
            })
            .collect();
        let find = |group: &str, plural: &str| {
            listable
                .iter()
                .find(|k| k.group() == group && k.plural() == plural)
                .copied()
        };
        let mut placed = std::collections::HashSet::new();
        let mut sections = Vec::new();
        for (title, entries) in SECTIONS {
            let entries: Vec<NavEntry> = entries
                .iter()
                .filter_map(|(g, p, label)| {
                    let kind = find(g, p)?;
                    placed.insert(kind.id());
                    Some(NavEntry::new(kind, label))
                })
                .collect();
            if !entries.is_empty() {
                sections.push(NavSection {
                    title: (*title).to_owned(),
                    kind: SectionKind::BuiltIn,
                    entries,
                });
            }
        }
        let mut other = Vec::new();
        let mut groups: BTreeMap<&str, Vec<NavEntry>> = BTreeMap::new();
        for kind in listable.iter().filter(|k| !placed.contains(&k.id())) {
            let entry = NavEntry::new(kind, &kind_label(kind.kind(), kind.plural()));
            if is_builtin_group(kind.group()) {
                other.push(entry);
            } else {
                groups.entry(kind.group()).or_default().push(entry);
            }
        }
        // Custom resources come before "Other" (rarely used built-ins), so they are reachable
        // without scrolling past every built-in kind.
        for (group, mut entries) in groups {
            entries.sort_by(|a, b| a.label.cmp(&b.label));
            sections.push(NavSection {
                title: group.to_owned(),
                kind: SectionKind::ApiGroup,
                entries,
            });
        }
        if !other.is_empty() {
            other.sort_by(|a, b| a.label.cmp(&b.label));
            sections.push(NavSection {
                title: "Other".to_owned(),
                kind: SectionKind::Other,
                entries: other,
            });
        }
        Navigation { sections }
    }

    pub fn entry(&self, id: &KindId) -> Option<&NavEntry> {
        self.sections
            .iter()
            .flat_map(|s| &s.entries)
            .find(|e| e.id == *id)
    }
}

/// A kind's label as the navigation shows it: its words spaced and the last one plural, checked
/// against the server's plural (`ComponentStatus`, `componentstatuses` → "Component Statuses";
/// `CSIDriver` → "CSI Drivers"). A last word that cannot be matched and already ends in "s"
/// stays as it is ("Pod Metrics").
pub fn kind_label(kind: &str, plural: &str) -> String {
    let words = split_words(kind);
    let Some((last, first)) = words.split_last() else {
        return plural.to_owned();
    };
    let head: String = first.concat().to_lowercase();
    let candidates = plural_forms(last);
    let last = candidates
        .iter()
        .find(|c| format!("{head}{}", c.to_lowercase()) == plural)
        .cloned()
        .unwrap_or_else(|| {
            if last.ends_with('s') {
                (*last).to_owned()
            } else {
                candidates[0].clone()
            }
        });
    let mut out: Vec<&str> = first.to_vec();
    out.push(&last);
    out.join(" ")
}

/// Splits `CamelCase` into words, keeping acronyms together (`CSIDriver` → `CSI`, `Driver`).
fn split_words(kind: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = kind.char_indices().collect();
    let mut words = Vec::new();
    let mut start = 0;
    for i in 1..chars.len() {
        let (at, c) = chars[i];
        let prev = chars[i - 1].1;
        let next_lower = chars.get(i + 1).is_some_and(|(_, n)| n.is_lowercase());
        let boundary = c.is_uppercase()
            && (prev.is_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_uppercase() && next_lower));
        if boundary {
            words.push(&kind[start..at]);
            start = at;
        }
    }
    if start < kind.len() {
        words.push(&kind[start..]);
    }
    words
}

/// English plural forms of a word, the most likely first.
fn plural_forms(word: &str) -> Vec<String> {
    let mut forms = Vec::new();
    let lower = word.to_lowercase();
    if lower.ends_with('y')
        && !lower.ends_with("ay")
        && !lower.ends_with("ey")
        && !lower.ends_with("oy")
    {
        forms.push(format!("{}ies", &word[..word.len() - 1]));
    }
    if ["s", "x", "z", "ch", "sh"]
        .iter()
        .any(|e| lower.ends_with(e))
    {
        forms.push(format!("{word}es"));
    }
    forms.push(format!("{word}s"));
    forms.push(word.to_owned());
    forms
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kube::api::ApiResource;

    pub(crate) fn kind(
        group: &str,
        kind: &str,
        plural: &str,
        namespaced: bool,
        verbs: &[&str],
    ) -> KindInfo {
        let version = "v1";
        let api_version = if group.is_empty() {
            version.to_owned()
        } else {
            format!("{group}/{version}")
        };
        KindInfo {
            resource: ApiResource {
                group: group.into(),
                version: version.into(),
                api_version,
                kind: kind.into(),
                plural: plural.into(),
            },
            namespaced,
            verbs: verbs.iter().map(|v| (*v).to_owned()).collect(),
            singular: kind.to_ascii_lowercase(),
            short_names: Vec::new(),
        }
    }

    const LW: &[&str] = &["get", "list", "watch"];

    // covers UP-22
    #[test]
    fn builtins_in_sections_crds_by_group() {
        let kinds = vec![
            kind("", "Pod", "pods", true, LW),
            kind("", "Binding", "bindings", true, &["create"]),
            kind(
                "",
                "ComponentStatus",
                "componentstatuses",
                false,
                &["get", "list"],
            ),
            kind("apps", "Deployment", "deployments", true, LW),
            kind("events.k8s.io", "Event", "events", true, LW),
            kind("rbac.authorization.k8s.io", "Role", "roles", true, LW),
            kind(
                "metrics.k8s.io",
                "PodMetrics",
                "pods",
                true,
                &["get", "list"],
            ),
            kind("stable.example.com", "Widget", "widgets", true, LW),
            kind("stable.example.com", "Gadget", "gadgets", false, LW),
            kind("cert-manager.io", "Certificate", "certificates", true, LW),
            kind("gateway.networking.k8s.io", "Gateway", "gateways", true, LW),
            kind("", "PodTemplate", "podtemplates", true, LW),
        ];
        let nav = Navigation::build(&kinds);
        let titles: Vec<&str> = nav.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Workloads",
                "Access Control",
                "cert-manager.io",
                "gateway.networking.k8s.io",
                "metrics.k8s.io",
                "stable.example.com",
                "Other",
            ]
        );
        let workloads: Vec<&str> = nav.sections[0]
            .entries
            .iter()
            .map(|e| e.label.as_str())
            .collect();
        assert_eq!(workloads, ["Pods", "Deployments"]);
        let other: Vec<&str> = nav.sections[6]
            .entries
            .iter()
            .map(|e| e.label.as_str())
            .collect();
        assert_eq!(
            other,
            ["Component Statuses", "Pod Templates"],
            "bindings is not listable"
        );
        let crds = &nav.sections[5].entries;
        assert_eq!(crds[0].label, "Gadgets");
        assert!(!crds[0].namespaced && crds[0].watchable && crds[0].listable);
        assert!(nav.entry(&KindId::new("events.k8s.io", "events")).is_none());
        let metrics = nav.entry(&KindId::new("metrics.k8s.io", "pods")).unwrap();
        assert!(metrics.listable && !metrics.watchable);
        assert_eq!(metrics.label, "Pod Metrics");
    }

    // covers UP-22
    #[test]
    fn every_kind_gets_a_spaced_plural_label() {
        for (kind, plural, label) in [
            ("Widget", "widgets", "Widgets"),
            (
                "ClusterTrustBundle",
                "clustertrustbundles",
                "Cluster Trust Bundles",
            ),
            ("ComponentStatus", "componentstatuses", "Component Statuses"),
            ("CSIDriver", "csidrivers", "CSI Drivers"),
            ("APIService", "apiservices", "API Services"),
            ("IPAddress", "ipaddresses", "IP Addresses"),
            ("NetworkPolicy", "networkpolicies", "Network Policies"),
            ("Gateway", "gateways", "Gateways"),
            ("PodMetrics", "pods", "Pod Metrics"),
            ("Endpoints", "endpoints", "Endpoints"),
            ("Ingress", "ingresses", "Ingresses"),
            ("V1Thing", "v1things", "V1 Things"),
        ] {
            assert_eq!(kind_label(kind, plural), label, "{kind}");
        }
    }

    // covers UP-10, UP-21
    #[test]
    fn resolves_typed_names_through_the_shared_resolver() {
        let mut deploy = kind("apps", "Deployment", "deployments", true, LW);
        deploy.short_names = vec!["deploy".into()];
        let discovery = ClusterDiscovery::new(vec![kind("", "Pod", "pods", true, LW), deploy]);
        for q in ["deploy", "deployment", "Deployment", "deployments.apps"] {
            assert_eq!(
                discovery.resolve(q).unwrap().id().as_str(),
                "deployments.apps",
                "{q}"
            );
        }
        assert!(matches!(
            discovery.resolve("nope"),
            Err(ResolveKindError::NotFound(_))
        ));
    }

    // covers UP-21
    #[test]
    fn nav_entries_for_typed_names() {
        let mut deploy = kind("apps", "Deployment", "deployments", true, LW);
        deploy.short_names = vec!["deploy".into()];
        let discovery = ClusterDiscovery::new(vec![
            kind("", "Pod", "pods", true, LW),
            kind("", "Binding", "bindings", true, &["create"]),
            deploy,
        ]);
        assert_eq!(
            discovery.nav_entry("deploy").unwrap().id.as_str(),
            "deployments.apps"
        );
        assert_eq!(discovery.nav_entry("PODS").unwrap().id.as_str(), "pods");
        assert_eq!(
            discovery.nav_entry("binding").unwrap_err(),
            NavEntryError::NotInNavigation(KindId::new("", "bindings"))
        );
        assert_eq!(
            discovery.nav_entry("binding").unwrap_err().to_string(),
            "bindings is served but cannot be listed, so it has no list to open"
        );
        assert!(matches!(
            discovery.nav_entry("sprocket"),
            Err(NavEntryError::Unresolved(ResolveKindError::NotFound(_)))
        ));
    }
}
