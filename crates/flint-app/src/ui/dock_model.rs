//! The pure parts of the bottom dock (#13; UP-42): which cluster a tab's session runs on and
//! whether its context still points there, how a tab is titled, and how the tab list names a
//! tab. No GPUI types, so all of it is testable without a window; the dock (`dock.rs`) draws
//! with them.

use std::collections::{HashMap, HashSet};

use flint_core::{ClusterId, display};

/// The longest tab title, in characters, before its middle is cut. The cluster chip is drawn
/// beside it ([`chip_labels`]); with both at their longest, two and a half tabs fit in a dock
/// 860 points wide (more as usual names are shorter).
pub const TITLE_CHARS: usize = 26;
/// The longest cluster name in a chip, in characters, before its middle is cut.
pub const CHIP_CHARS: usize = 16;
/// The longest part a chip adds to tell two clusters with the same name apart.
const CHIP_EXTRA_CHARS: usize = 12;

/// How a tab's session is doing, shown as a dot on the tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabStatus {
    Starting,
    Live,
    Retrying,
    Ended,
    Failed,
}

impl TabStatus {
    /// The status in a word, for the tab list.
    pub fn word(self) -> &'static str {
        match self {
            TabStatus::Starting => "starting",
            TabStatus::Live => "live",
            TabStatus::Retrying => "reconnecting",
            TabStatus::Ended => "ended",
            TabStatus::Failed => "failed",
        }
    }
}

/// Whether an open tab in `status` answers for its request, so opening the request again shows
/// it: while its session starts, runs or retries, and once a read that ends on its own (previous
/// logs) has ended. A tab that failed, or a stream or shell that ended, keeps its output, and
/// opening the request again opens a fresh tab beside it.
pub fn reopens(status: TabStatus, reads_to_end: bool) -> bool {
    match status {
        TabStatus::Starting | TabStatus::Live | TabStatus::Retrying => true,
        TabStatus::Ended => reads_to_end,
        TabStatus::Failed => false,
    }
}

/// Whether closing a tab in `status` ends a session still at work: a shell, or a log tab
/// that follows its stream, while it starts, runs or reconnects. A tab that ended or failed
/// holds only output, and previous logs (`reads_to_end`) end on their own.
pub fn ends_a_session(status: TabStatus, reads_to_end: bool) -> bool {
    !reads_to_end
        && matches!(
            status,
            TabStatus::Starting | TabStatus::Live | TabStatus::Retrying
        )
}

/// The sessions closing the window or quitting would end ([`ends_a_session`]), by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveSessions {
    pub shells: usize,
    pub logs: usize,
}

impl LiveSessions {
    pub fn total(self) -> usize {
        self.shells + self.logs
    }

    pub fn is_empty(self) -> bool {
        self.total() == 0
    }

    /// "1 shell", "2 log streams", "1 shell and 3 log streams".
    pub fn described(self) -> String {
        let count = |n: usize, one: &str, many: &str| match n {
            1 => format!("1 {one}"),
            n => format!("{n} {many}"),
        };
        let shells = count(self.shells, "shell", "shells");
        let logs = count(self.logs, "log stream", "log streams");
        match (self.shells, self.logs) {
            (_, 0) => shells,
            (0, _) => logs,
            _ => format!("{shells} and {logs}"),
        }
    }
}

impl std::ops::AddAssign for LiveSessions {
    fn add_assign(&mut self, other: Self) {
        self.shells += other.shells;
        self.logs += other.logs;
    }
}

/// The cluster a tab's session runs on: the kubeconfig context and the API server it pointed at
/// when the tab opened. Like an AI exposure (ADR-0009) and a trusted cluster (ADR-0015), a tab is
/// pinned to both, so a context re-pointed at another server is another cluster: opening the
/// same pod there opens a new tab instead of showing the old server's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TabCluster {
    pub context: ClusterId,
    /// The server URL without a trailing slash; `None` when the context names no cluster entry.
    pub server: Option<String>,
    /// The context's confirmed definition when the tab opened, as far as its session depends on
    /// it: [`ClusterSummary::definition`] for a shell, [`ClusterSummary::endpoint`] for logs, and
    /// `0` for agent activity, which reads a file. A context moved and put back is another
    /// definition at the same server, so the tab is not reused for it; an edit that keeps the
    /// server keeps the definition.
    ///
    /// [`ClusterSummary::definition`]: flint_core::ClusterSummary::definition
    /// [`ClusterSummary::endpoint`]: flint_core::ClusterSummary::endpoint
    definition: u64,
}

impl TabCluster {
    pub fn new(context: impl Into<ClusterId>, server: Option<&str>) -> Self {
        TabCluster {
            context: context.into(),
            server: server.map(normalize_server),
            definition: 0,
        }
    }

    /// This tab cluster, opened on the context's confirmed `definition`.
    pub fn defined_by(mut self, definition: u64) -> Self {
        self.definition = definition;
        self
    }

    /// Whether the context, now pointing at `server`, is still this tab's cluster.
    pub fn is_at(&self, server: Option<&str>) -> bool {
        self.server.as_deref() == server.map(normalize_server).as_deref()
    }
}

/// The server of each context whose server the core has confirmed. One that may be a fallback,
/// showing only while a kubeconfig file can't be read, is left out, so its shells keep running.
pub fn confirmed_servers(
    clusters: &[flint_core::ClusterSummary],
) -> HashMap<String, Option<String>> {
    clusters
        .iter()
        .filter(|c| c.confirmed)
        .map(|c| (c.info.name.clone(), c.info.server.clone()))
        .collect()
}

/// What a context's chip says, for every context in kubeconfig: its cluster's name, short (an
/// EKS ARN's cluster name, a GKE context's cluster, else the context middle-cut to
/// [`CHIP_CHARS`]), with what tells it apart added wherever two contexts would read the same:
/// the account or region (EKS), the project or location (GKE), or else the part of the context
/// names that differs. No two contexts get the same chip, so a production shell never reads
/// like a staging one.
pub fn chip_labels(contexts: &[String]) -> HashMap<String, String> {
    let unique: Vec<&String> = {
        let mut seen = HashSet::new();
        contexts.iter().filter(|c| seen.insert(*c)).collect()
    };
    let parts: Vec<(String, Vec<String>)> = unique.iter().map(|c| chip_parts(c)).collect();
    let mut labels: Vec<String> = parts
        .iter()
        .map(|(name, _)| display::middle_truncate(name, CHIP_CHARS))
        .collect();
    for group in collisions(&labels) {
        // The first field (account, project, ...) that tells every one of them apart.
        let fields = parts[group[0]].1.len();
        let field = (0..fields).find(|&f| {
            let values: HashSet<Option<&String>> =
                group.iter().map(|&ix| parts[ix].1.get(f)).collect();
            values.len() == group.len() && values.iter().all(Option::is_some)
        });
        // Of that field, or else of the whole names, the part that differs.
        let names: Vec<&str> = match field {
            Some(f) => group.iter().map(|&ix| parts[ix].1[f].as_str()).collect(),
            None => group.iter().map(|&ix| unique[ix].as_str()).collect(),
        };
        let differs = differing_parts(&names);
        for (n, &ix) in group.iter().enumerate() {
            let extra = &differs[n];
            labels[ix] = format!(
                "{} · {}",
                labels[ix],
                display::middle_truncate(extra, CHIP_EXTRA_CHARS)
            );
        }
    }
    // Whatever still reads the same (parts that differ only past the cut) gets numbered.
    for group in collisions(&labels) {
        for (n, &ix) in group.iter().enumerate().skip(1) {
            labels[ix] = format!("{} #{}", labels[ix], n + 1);
        }
    }
    unique.into_iter().cloned().zip(labels).collect()
}

/// The positions of labels that read the same, in groups of two or more.
fn collisions(labels: &[String]) -> Vec<Vec<usize>> {
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for (ix, label) in labels.iter().enumerate() {
        groups.entry(label).or_default().push(ix);
    }
    let mut groups: Vec<Vec<usize>> = groups.into_values().filter(|g| g.len() > 1).collect();
    groups.sort();
    groups
}

/// A context's cluster name and the fields that tell clusters of that name apart, most telling
/// first.
fn chip_parts(context: &str) -> (String, Vec<String>) {
    let eks = context
        .strip_prefix("arn:aws:eks:")
        .or_else(|| context.strip_prefix("arn:aws-cn:eks:"))
        .or_else(|| context.strip_prefix("arn:aws-us-gov:eks:"))
        .and_then(|rest| {
            let mut parts = rest.splitn(3, ':');
            let region = parts.next()?;
            let account = parts.next()?;
            let name = parts.next()?.strip_prefix("cluster/")?;
            (!name.is_empty()).then(|| (name, vec![account.to_owned(), region.to_owned()]))
        });
    if let Some((name, fields)) = eks {
        return (name.to_owned(), fields);
    }
    // gke_<project>_<location>_<cluster>
    let parts: Vec<&str> = context.splitn(4, '_').collect();
    if let ["gke", project, location, name] = parts.as_slice()
        && !name.is_empty()
    {
        return (
            (*name).to_owned(),
            vec![(*project).to_owned(), (*location).to_owned()],
        );
    }
    (context.to_owned(), Vec::new())
}

/// For each name, the part that differs from the others: what is left once the prefix and the
/// suffix they all share are taken off, widened to whole words.
fn differing_parts(names: &[&str]) -> Vec<String> {
    let chars: Vec<Vec<char>> = names.iter().map(|n| n.chars().collect()).collect();
    let shortest = chars.iter().map(Vec::len).min().unwrap_or(0);
    let separator = |c: char| matches!(c, '-' | '_' | ':' | '/' | '.' | '@');
    let mut prefix = (0..shortest)
        .take_while(|&i| chars.iter().all(|c| c[i] == chars[0][i]))
        .count();
    while prefix > 0 && !separator(chars[0][prefix - 1]) {
        prefix -= 1;
    }
    let mut suffix = (0..shortest - prefix)
        .take_while(|&i| {
            chars
                .iter()
                .all(|c| c[c.len() - 1 - i] == chars[0][chars[0].len() - 1 - i])
        })
        .count();
    while suffix > 0 && !separator(chars[0][chars[0].len() - suffix]) {
        suffix -= 1;
    }
    chars
        .iter()
        .map(|c| {
            let part: String = c[prefix..c.len() - suffix].iter().collect();
            let part = part.trim_matches(separator);
            if part.is_empty() {
                "…".to_owned()
            } else {
                part.to_owned()
            }
        })
        .collect()
}

/// What a chip's tooltip says: the full context and its API server.
pub fn chip_tooltip(cluster: &TabCluster) -> String {
    let server = match &cluster.server {
        Some(server) => flint_core::redact::server_origin(server),
        None => "no API server".to_owned(),
    };
    format!("Context {}\nAPI server {server}", cluster.context)
}

/// `https://host:6443/` and `https://host:6443` are the same server.
fn normalize_server(server: &str) -> String {
    server.trim_end_matches('/').to_owned()
}

/// A tab's title as drawn: cut in the middle when long, so both the pod's name and its container
/// stay readable.
pub fn shown_title(title: &str) -> String {
    display::middle_truncate(title, TITLE_CHARS)
}

/// What the tab list says of a tab: its title, its cluster and its status.
pub fn list_label(title: &str, cluster: &str, status: &str) -> String {
    format!("{} — {cluster} · {status}", shown_title(title))
}

/// What a tab's accessible label and full name say: its title and its cluster.
pub fn full_title(title: &str, cluster: &TabCluster) -> String {
    format!("{title} on {}", cluster.context)
}

/// Why a shell ended when its context was re-pointed at another server.
pub fn repointed_note(context: &str) -> String {
    format!(
        "The context {context} now points at a different API server, so this shell was ended: \
         it ran on the old one. Open a shell again to reach the new server."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers UP-41: a server the core has not confirmed may be a fallback, so it is not a reason
    // to end a shell.
    #[test]
    fn only_confirmed_servers_can_end_a_shell() {
        let summary = |name: &str, confirmed| flint_core::ClusterSummary {
            info: flint_core::kubeconfig::ContextInfo {
                name: name.into(),
                cluster: None,
                user: None,
                namespace: None,
                server: Some("https://elsewhere".into()),
                proxy_url: None,
                source: "/k".into(),
            },
            state: flint_core::ConnectionState::Disconnected,
            generation: 1,
            definition: 1,
            endpoint: 1,
            write_epoch: 0,
            confirmed,
            provisional: !confirmed,
            paused: false,
        };
        let servers = confirmed_servers(&[summary("sure", true), summary("fallback", false)]);
        assert_eq!(
            servers.get("sure"),
            Some(&Some("https://elsewhere".to_owned()))
        );
        assert!(!servers.contains_key("fallback"));
    }

    // covers UP-42: a tab is pinned to its context and the server it pointed at.
    #[test]
    fn a_tab_cluster_is_its_context_and_server() {
        let tab = TabCluster::new("prod", Some("https://10.0.0.1:6443/"));
        assert_eq!(tab.server.as_deref(), Some("https://10.0.0.1:6443"));
        assert!(tab.is_at(Some("https://10.0.0.1:6443")));
        assert!(tab.is_at(Some("https://10.0.0.1:6443/")));
        assert!(!tab.is_at(Some("https://10.0.0.2:6443")));
        assert!(!tab.is_at(None));
        assert_ne!(tab, TabCluster::new("prod", Some("https://10.0.0.2:6443")));
        assert!(TabCluster::new("x", None).is_at(None));
    }

    // covers UP-42: opening a request again shows its tab while the session lives; an ended
    // or failed one opens a fresh tab (previous logs, read to their end, are shown again).
    #[test]
    fn only_live_tabs_answer_for_their_request() {
        use TabStatus::*;
        for status in [Starting, Live, Retrying] {
            assert!(reopens(status, false), "{status:?}");
        }
        assert!(!reopens(Ended, false));
        assert!(reopens(Ended, true));
        assert!(!reopens(Failed, false));
        assert!(!reopens(Failed, true));
    }

    // covers UP-42 (#177): closing the window or quitting asks while a shell or a following
    // log tab is at work; ended tabs, failed ones and previous logs end nothing.
    #[test]
    fn only_working_sessions_count_as_live() {
        use TabStatus::*;
        for status in [Starting, Live, Retrying] {
            assert!(ends_a_session(status, false), "{status:?}");
            assert!(!ends_a_session(status, true), "{status:?} previous logs");
        }
        for status in [Ended, Failed] {
            assert!(!ends_a_session(status, false), "{status:?}");
        }
    }

    #[test]
    fn live_sessions_name_their_kinds() {
        let live = |shells, logs| LiveSessions { shells, logs };
        assert!(live(0, 0).is_empty());
        assert_eq!(live(1, 0).described(), "1 shell");
        assert_eq!(live(2, 0).described(), "2 shells");
        assert_eq!(live(0, 1).described(), "1 log stream");
        assert_eq!(live(1, 3).described(), "1 shell and 3 log streams");
        let mut sum = live(1, 0);
        sum += live(1, 2);
        assert_eq!(sum, live(2, 2));
        assert_eq!(sum.total(), 4);
    }

    // covers UP-42: tabs name their cluster in its short form, and long titles keep both ends.
    #[test]
    fn titles_and_labels_stay_short() {
        let long = "Shell: checkout-api-7d9f8b6c5d-x2x9z · istio-proxy";
        let shown = shown_title(long);
        assert_eq!(shown.chars().count(), TITLE_CHARS);
        assert!(shown.starts_with("Shell: check"), "{shown}");
        assert!(shown.ends_with("istio-proxy"), "{shown}");
        assert_eq!(shown_title("web-1 · app"), "web-1 · app");
        assert_eq!(
            list_label("Shell: web-1 · app", "kind-dev", TabStatus::Live.word()),
            "Shell: web-1 · app — kind-dev · live"
        );
        assert_eq!(
            full_title("api (Deployment)", &TabCluster::new("kind-dev", None)),
            "api (Deployment) on kind-dev"
        );
    }

    fn labels(contexts: &[&str]) -> Vec<String> {
        let owned: Vec<String> = contexts.iter().map(|c| (*c).to_owned()).collect();
        let labels = chip_labels(&owned);
        contexts.iter().map(|c| labels[*c].clone()).collect()
    }

    // covers UP-42: a chip names its cluster briefly, and two contexts never share a chip: the
    // account, the project, or the part of the names that differs tells them apart.
    #[test]
    fn chips_tell_clusters_apart() {
        assert_eq!(
            labels(&[
                "kind-flint-dev",
                "arn:aws:eks:eu-west-1:123456789012:cluster/payments-prod",
            ]),
            ["kind-flint-dev", "payments-prod"],
            "short names as they are; no cluster/ prefix"
        );
        assert_eq!(
            labels(&[
                "arn:aws:eks:eu-west-1:111111111111:cluster/main",
                "arn:aws:eks:eu-west-1:222222222222:cluster/main",
                "arn:aws:eks:us-east-1:111111111111:cluster/other",
            ]),
            ["main · 111111111111", "main · 222222222222", "other"]
        );
        assert_eq!(
            labels(&[
                "arn:aws:eks:eu-west-1:111111111111:cluster/main",
                "arn:aws:eks:us-east-1:111111111111:cluster/main",
            ]),
            ["main · eu-west", "main · us-east"],
            "same account: the region"
        );
        assert_eq!(
            labels(&[
                "gke_acme-platform-prod_europe-west1-b_core",
                "gke_acme-platform-dev_europe-west1-b_core",
            ]),
            ["core · prod", "core · dev"]
        );
        // Plain names cut alike: the differing word is added.
        assert_eq!(
            labels(&[
                "team-payments-production-eu-cluster",
                "team-payments-staging-eu-cluster",
            ]),
            [
                "team-pa…-cluster · production",
                "team-pa…-cluster · staging"
            ]
        );
        // Contexts that only differ past every cut still get distinct chips.
        let same = labels(&[
            "gke_a_b_c",
            "gke_a_b_c",
            "x-very-long-shared-prefix-aaaaaaaaaaaaaaaaaaaaaaaa1-tail-tail-tail",
            "x-very-long-shared-prefix-aaaaaaaaaaaaaaaaaaaaaaaa2-tail-tail-tail",
        ]);
        assert_eq!(same[0], same[1], "one context listed twice is one chip");
        assert_ne!(same[2], same[3]);
        assert_eq!(
            chip_tooltip(&TabCluster::new("prod", Some("https://u:p@10.0.0.1:6443"))),
            "Context prod\nAPI server https://10.0.0.1:6443"
        );
        assert_eq!(
            chip_tooltip(&TabCluster::new(
                "prod",
                Some("https://10.0.0.1:6443/k8s/c-1?x=1")
            )),
            "Context prod\nAPI server https://10.0.0.1:6443"
        );
    }
}
