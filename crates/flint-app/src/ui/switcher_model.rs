//! The resource switcher's search (Cmd-P, UP-31), apart from drawing: an index of the kinds the
//! active cluster serves and the objects its watches hold in memory, and a fuzzy search over it.
//!
//! The index is built once per opening, off the UI thread, from snapshots the core reads
//! without any request (`CoreHandle::stored_snapshots`). The search runs once per keystroke,
//! never while drawing: it goes on from the last keystroke's matches when the query only grew,
//! rules most objects out with the subsequence check before any scoring, ranks them with a
//! linear-time score that allocates nothing ([`Matcher::quick_points`]), and gives only the best
//! [`RESULTS`] the full scoring, for their order and highlights.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use flint_core::{KindId, KindSnapshot, NavEntry};

use super::palette_model::{ALT_PENALTY, Matcher};

/// At most this many results are listed; typing more narrows them.
pub const RESULTS: usize = 200;

/// A kind the switcher offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind {
    pub id: KindId,
    /// As the navigation shows it: Deployments.
    pub label: String,
}

/// An object of a kind, as the list finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub namespace: Option<Arc<str>>,
    pub name: Box<str>,
}

/// One thing the switcher can open: a kind's list, or an object in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Its kind, an index into [`Index::kinds`].
    pub kind: u32,
    /// `None` for the kind itself.
    pub object: Option<Object>,
    /// What is matched and shown: the kind's label, or `namespace/name` (`name` when
    /// cluster-scoped).
    pub text: Box<str>,
    /// Also matched, with less weight: a kind's id (`deployments.apps`), an object's kind and
    /// text (`Pods demo/web`), so "pods web" finds the pod web.
    pub alt: Box<str>,
    /// Whether `text` is ASCII (names almost always are), known once rather than per keystroke.
    ascii: bool,
}

/// Everything the switcher searches: the kinds first, in navigation order, then the objects by
/// kind, namespace and name.
#[derive(Debug, Clone, Default)]
pub struct Index {
    pub kinds: Vec<Kind>,
    pub items: Vec<Item>,
    /// How many of the items are objects.
    pub objects: usize,
}

impl Index {
    /// The listable kinds among `entries` (the navigation's), and the rows of `snapshots` of
    /// those kinds. Objects of kinds the navigation does not list could not be opened, so they
    /// are left out.
    pub fn new<'a>(
        entries: impl IntoIterator<Item = &'a NavEntry>,
        snapshots: &[Arc<KindSnapshot>],
    ) -> Self {
        let mut index = Index::default();
        let mut by_id: HashMap<KindId, u32> = HashMap::new();
        for entry in entries.into_iter().filter(|e| e.listable) {
            if by_id.contains_key(&entry.id) {
                continue;
            }
            let kind = index.kinds.len() as u32;
            by_id.insert(entry.id.clone(), kind);
            index.items.push(Item {
                kind,
                object: None,
                text: entry.label.as_str().into(),
                alt: format!("{} {}", entry.id, entry.kind).into(),
                ascii: entry.label.is_ascii(),
            });
            index.kinds.push(Kind {
                id: entry.id.clone(),
                label: entry.label.clone(),
            });
        }
        for snapshot in snapshots {
            let Some(&kind) = by_id.get(&snapshot.kind) else {
                continue;
            };
            let label = &index.kinds[kind as usize].label;
            for row in &snapshot.rows {
                let text: Box<str> = match &row.namespace {
                    Some(ns) => format!("{ns}/{}", row.name).into(),
                    None => row.name.clone(),
                };
                // `Matcher::matches_rest` relies on this shape: the label, a space, the text.
                index.items.push(Item {
                    kind,
                    object: Some(Object {
                        namespace: row.namespace.clone(),
                        name: row.name.clone(),
                    }),
                    alt: format!("{label} {text}").into(),
                    ascii: text.is_ascii(),
                    text,
                });
                index.objects += 1;
            }
        }
        index
    }

    /// The kind of `item`.
    pub fn kind_of(&self, item: &Item) -> &Kind {
        &self.kinds[item.kind as usize]
    }
}

/// A listed item: its index in [`Index::items`], and the matched bytes of its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub item: u32,
    pub ranges: Vec<Range<usize>>,
}

/// What a query found: the best hits (at most [`RESULTS`]), best first, and how many items
/// matched in all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub hits: Vec<Hit>,
    pub matched: usize,
}

/// A search over one [`Index`] as the query is typed. It remembers the last query's matches,
/// so typing more searches only those. Start a new one for another index.
#[derive(Debug, Clone, Default)]
pub struct Search {
    /// The last query and every item it matched (not only the ones listed).
    last: Option<(Matcher, Vec<u32>)>,
}

impl Search {
    /// The items matching `query`, best first. With no query: the kinds, then the objects, in
    /// index order. With one: by score, so a closer match always wins; a match of the text beats
    /// the same match of the second text; among equal scores, index order (kinds first).
    pub fn run(&mut self, index: &Index, query: &str) -> Found {
        let mut matcher = Matcher::new(query);
        if matcher.is_empty() {
            self.last = None;
            return Found {
                hits: (0..index.items.len().min(RESULTS))
                    .map(|i| Hit {
                        item: i as u32,
                        ranges: Vec::new(),
                    })
                    .collect(),
                matched: index.items.len(),
            };
        }
        let earlier = match self.last.take() {
            Some((earlier, matched)) if matcher.extends(&earlier) => Some(matched),
            _ => None,
        };
        // How much of the query each kind's label takes, matched greedily: an object's second
        // text is its kind's label then its own text, so it can match only if the rest of the
        // query is in its text. Most objects whose text failed are ruled out without reading
        // their second text.
        let label_takes: Vec<usize> = index
            .kinds
            .iter()
            .map(|k| matcher.prefix_len(&k.label))
            .collect();
        let mut matched: Vec<u32> = Vec::new();
        // (points, item, whether the text itself matched)
        let mut scored: Vec<(i32, u32, bool)> = Vec::new();
        // Ranked by the quick score; only the best RESULTS get the full scoring below.
        let mut consider = |ix: u32, matcher: &mut Matcher| {
            let item = &index.items[ix as usize];
            let found = match matcher.quick_points_of(&item.text, item.ascii) {
                Some(points) => Some((points, true)),
                None => {
                    let alt_possible = item.object.is_none() || {
                        let taken = label_takes[item.kind as usize];
                        taken > 0 && matcher.matches_rest(taken, &item.text)
                    };
                    alt_possible
                        .then(|| matcher.quick_points(&item.alt))
                        .flatten()
                        .map(|points| (points - ALT_PENALTY, false))
                }
            };
            if let Some((points, own)) = found {
                matched.push(ix);
                scored.push((points, ix, own));
            }
        };
        match earlier {
            Some(earlier) => earlier
                .into_iter()
                .for_each(|ix| consider(ix, &mut matcher)),
            None => (0..index.items.len() as u32).for_each(|ix| consider(ix, &mut matcher)),
        }
        let total = scored.len();
        let order = |a: &(i32, u32, bool)| (std::cmp::Reverse(a.0), a.1);
        if scored.len() > RESULTS {
            scored.select_nth_unstable_by_key(RESULTS - 1, order);
            scored.truncate(RESULTS);
        }
        // The best alignment of each one listed: its order, and the bytes to highlight.
        let mut listed: Vec<(i32, Hit)> = scored
            .into_iter()
            .map(|(quick, ix, own)| {
                let item = &index.items[ix as usize];
                let (points, ranges) = if own {
                    matcher
                        .score(&item.text)
                        .map_or((quick, Vec::new()), |s| (s.points, s.ranges))
                } else {
                    let points = matcher.points(&item.alt).map_or(quick, |p| p - ALT_PENALTY);
                    (points, Vec::new())
                };
                (points, Hit { item: ix, ranges })
            })
            .collect();
        listed.sort_unstable_by_key(|(points, hit)| (std::cmp::Reverse(*points), hit.item));
        let hits = listed.into_iter().map(|(_, hit)| hit).collect();
        self.last = Some((matcher, matched));
        Found {
            hits,
            matched: total,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use flint_core::{KindId, KindSnapshot, NavEntry, Row, SyncState};

    use super::{Index, RESULTS, Search};

    fn entry(group: &str, plural: &str, kind: &str, label: &str) -> NavEntry {
        NavEntry {
            id: KindId::new(group, plural),
            label: label.into(),
            kind: kind.into(),
            api_version: "v1".into(),
            namespaced: true,
            listable: true,
            watchable: true,
        }
    }

    fn snapshot(kind: KindId, rows: &[(Option<&str>, &str)]) -> Arc<KindSnapshot> {
        Arc::new(KindSnapshot {
            cluster: "alpha".into(),
            kind,
            rows: rows
                .iter()
                .map(|(ns, name)| {
                    Arc::new(Row {
                        uid: format!("{ns:?}/{name}").into(),
                        name: (*name).into(),
                        namespace: ns.map(Arc::from),
                        resource_version: "1".into(),
                        generation: None,
                        created: None,
                        labels: Box::new([]),
                        cells: Box::new([]),
                    })
                })
                .collect(),
            state: SyncState::Synced,
            ..Default::default()
        })
    }

    fn texts(index: &Index, search: &mut Search, query: &str) -> Vec<String> {
        search
            .run(index, query)
            .hits
            .iter()
            .map(|h| index.items[h.item as usize].text.to_string())
            .collect()
    }

    fn sample() -> Index {
        let entries = [
            entry("", "pods", "Pod", "Pods"),
            entry("apps", "deployments", "Deployment", "Deployments"),
            entry("", "nodes", "Node", "Nodes"),
        ];
        Index::new(
            &entries,
            &[
                snapshot(
                    KindId::pods(),
                    &[(Some("demo"), "web-1"), (Some("kube-system"), "coredns")],
                ),
                snapshot(KindId::new("apps", "deployments"), &[(Some("demo"), "web")]),
                snapshot(KindId::new("", "nodes"), &[(None, "worker-1")]),
                // Not in the navigation: left out.
                snapshot(KindId::new("", "events"), &[(Some("demo"), "web.1")]),
            ],
        )
    }

    // covers UP-31: kinds first, then every object, as namespace/name (cluster-scoped: name).
    #[test]
    fn indexes_kinds_then_objects_of_listed_kinds() {
        let index = sample();
        assert_eq!(index.kinds.len(), 3);
        assert_eq!(index.objects, 4);
        let all = texts(&index, &mut Search::default(), "");
        assert_eq!(
            all,
            [
                "Pods",
                "Deployments",
                "Nodes",
                "demo/web-1",
                "kube-system/coredns",
                "demo/web",
                "worker-1"
            ]
        );
        let web = &index.items[5];
        assert_eq!(index.kind_of(web).label, "Deployments");
        let object = web.object.as_ref().unwrap();
        assert_eq!(object.namespace.as_deref(), Some("demo"));
        assert_eq!(&*object.name, "web");
        assert!(index.items[6].object.as_ref().unwrap().namespace.is_none());
        // An unlistable kind is not offered.
        let mut hidden = entry("", "bindings", "Binding", "Bindings");
        hidden.listable = false;
        assert!(Index::new(&[hidden], &[]).items.is_empty());
    }

    // covers UP-31: loose matching, the closer match first, and the kind's name as a second
    // text: "pods web" finds the pod named web-1, not the deployment.
    #[test]
    fn matches_names_loosely_and_kinds_by_their_names() {
        let index = sample();
        let mut search = Search::default();
        // The kind's own name beats a deployment found by its kind.
        assert_eq!(
            texts(&index, &mut search, "deploy"),
            ["Deployments", "demo/web"]
        );
        assert_eq!(
            texts(&index, &mut search, "coredns"),
            ["kube-system/coredns"]
        );
        // Equal matches keep the index order.
        assert_eq!(
            texts(&index, &mut search, "web"),
            ["demo/web-1", "demo/web"]
        );
        assert_eq!(texts(&index, &mut search, "pods web"), ["demo/web-1"]);
        assert_eq!(
            texts(&index, &mut search, "deployments.apps"),
            ["Deployments"]
        );
        let found = search.run(&index, "zzz");
        assert!(found.hits.is_empty());
        assert_eq!(found.matched, 0);
        // The matched bytes of the text, for highlighting.
        let found = search.run(&index, "coredns");
        assert_eq!(found.hits[0].ranges, vec![12..19_usize]);
    }

    // covers UP-31: typing more searches only the earlier matches, with the same results as
    // starting over; deleting starts over.
    #[test]
    fn typing_more_narrows_the_earlier_matches() {
        let index = sample();
        let mut typed = Search::default();
        for query in ["w", "we", "web", "web-", "web-1", "web", "w", "", "co"] {
            let fresh = Search::default().run(&index, query);
            assert_eq!(typed.run(&index, query), fresh, "{query}");
        }
    }

    // covers UP-31: at most RESULTS are listed, the best ones, with the count of all matches.
    #[test]
    fn lists_at_most_the_best_results() {
        let names: Vec<String> = (0..RESULTS + 50).map(|i| format!("api-{i}")).collect();
        let mut rows: Vec<(Option<&str>, &str)> =
            names.iter().map(|n| (Some("demo"), n.as_str())).collect();
        rows.push((Some("demo"), "api"));
        let index = Index::new(
            &[entry("", "pods", "Pod", "Pods")],
            &[snapshot(KindId::pods(), &rows)],
        );
        let mut search = Search::default();
        let found = search.run(&index, "api");
        assert_eq!(found.matched, RESULTS + 51);
        assert_eq!(found.hits.len(), RESULTS);
        // Equal matches keep the index order; one with a gap ranks after them all.
        assert_eq!(
            &*index.items[found.hits[0].item as usize].text,
            "demo/api-0"
        );
        let gapped = search.run(&index, "ap1");
        assert!(gapped.matched > 0);
        let empty = search.run(&index, "");
        assert_eq!(empty.hits.len(), RESULTS);
        assert_eq!(empty.matched, RESULTS + 52);
    }

    /// 50,000 objects across 40 kinds and 200 namespaces, named as real clusters name them.
    fn large() -> Index {
        let entries: Vec<NavEntry> = (0..40)
            .map(|k| {
                entry(
                    "example.com",
                    &format!("kind{k}s"),
                    "Kind",
                    &format!("Kind {k}"),
                )
            })
            .collect();
        let snapshots: Vec<Arc<KindSnapshot>> = entries
            .iter()
            .enumerate()
            .map(|(k, e)| {
                let names: Vec<(String, String)> = (0..1250)
                    .map(|i| {
                        let n = k * 1250 + i;
                        (
                            format!("team-{}", n % 200),
                            format!("payments-api-{n:05x}-{}", n % 7),
                        )
                    })
                    .collect();
                let rows: Vec<(Option<&str>, &str)> = names
                    .iter()
                    .map(|(ns, name)| (Some(ns.as_str()), name.as_str()))
                    .collect();
                snapshot(e.id.clone(), &rows)
            })
            .collect();
        Index::new(&entries, &snapshots)
    }

    // covers UP-31: typing a name letter by letter over 50,000 objects stays within a frame.
    // About 16 ms per keystroke at worst in this unoptimized test build here; the limit is 50 ms
    // so a shared CI runner does not fail it, while a quadratic slowdown or a lost pre-check
    // still would.
    #[test]
    fn searching_fifty_thousand_objects_fits_in_a_frame() {
        let index = large();
        assert_eq!(index.objects, 50_000);
        let query = "team-17/payments-api-00bc9";
        let mut search = Search::default();
        let mut slowest = Duration::ZERO;
        for end in 1..=query.len() {
            // The best of three, from the same state, so a busy machine's pauses do not count.
            let before = search.clone();
            let mut spent = Duration::MAX;
            for _ in 0..3 {
                let mut attempt = before.clone();
                let start = Instant::now();
                let found = attempt.run(&index, &query[..end]);
                spent = spent.min(start.elapsed());
                assert!(!found.hits.is_empty(), "{}", &query[..end]);
                search = attempt;
            }
            slowest = slowest.max(spent);
        }
        let found = search.run(&index, query);
        assert_eq!(
            &*index.items[found.hits[0].item as usize].text,
            "team-17/payments-api-00bc9-0"
        );
        assert!(slowest < Duration::from_millis(50), "{slowest:?}");
    }
}
