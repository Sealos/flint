//! The resource switcher's search (Cmd-P, UP-31), apart from drawing: an index of the kinds the
//! active cluster serves and the objects its watches hold in memory, and a fuzzy search over it.
//!
//! The index is built once per opening, off the UI thread, from snapshots the core reads
//! without any request (`CoreHandle::stored_snapshots`). The search runs once per keystroke,
//! never while drawing: it goes on from the last keystroke's matches when the query only grew,
//! rules most objects out with the subsequence check before any scoring, ranks them with a
//! linear-time score that allocates nothing ([`Matcher::quick_points`]), and gives only the best
//! [`RESULTS`] the full scoring, for their order and highlights.
//!
//! Two rules keep the list relevant: objects matching only by scattered letters are left out
//! when another object of the same sort (Event, or not) has the query as a run
//! ([`RunFinder`]), and Event objects, which repeat the name of what they are about, come after
//! every other match.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use flint_core::{KindId, KindSnapshot, NavEntry};

use super::palette_model::{ALT_PENALTY, Matcher};

/// At most this many results are listed; typing more narrows them.
pub const RESULTS: usize = 200;
/// Extra points when the query is the whole name (the kind's label, or the object's name
/// without its namespace), ignoring case: Deployment `demo/web` before ConfigMap
/// `demo/web-cfg` for "web".
const EXACT_NAME: i32 = 200;
/// Extra points when the name starts with the query.
const NAME_PREFIX: i32 = 100;
/// Extra points when the query is found in the name alone, not across the namespace.
const IN_NAME: i32 = 16;

/// A kind the switcher offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind {
    pub id: KindId,
    /// As the navigation shows it: Deployments.
    pub label: String,
    /// How many of its objects are in memory; `None` when its watch has not listed it (not
    /// watched now, or still loading).
    pub in_memory: Option<usize>,
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
    /// Whether this is an Event object, listed after every other match. The Events kind itself
    /// is not one.
    event: bool,
    /// Where the name starts in `text`: after `namespace/`.
    name_at: u32,
}

impl Item {
    /// The name: the kind's label, or the object's name without its namespace.
    pub fn name(&self) -> &str {
        &self.text[self.name_at as usize..]
    }
}

/// The extra points of `item`, whose text matched: [`IN_NAME`] when the query is found in the
/// name alone (always, when the text is the name: a kind, a cluster-scoped object), plus
/// [`name_bonus`]. `start` is where the quick match began: at or past the name, the query is
/// in the name, so ranking very many objects needs no second scan. The cut to the best
/// [`RESULTS`] is therefore approximate: a query that also matches across the namespace (`ab`
/// in `ab/cab`, `api` in `api-gateway/gateway-api`) starts in the namespace, so the object
/// misses [`IN_NAME`] there and may fall just below the cut. Only the final order (`exact`)
/// checks the name itself.
fn name_points(matcher: &mut Matcher, item: &Item, query: &str, start: usize, exact: bool) -> i32 {
    let in_name = item.name_at == 0
        || start >= item.name_at as usize
        || (exact
            // A name has no "/" (it splits namespace and name): a query with one is never in it.
            && !query.contains('/')
            && matcher.matches_rest(0, item.name()));
    let in_name = if in_name { IN_NAME } else { 0 };
    in_name + name_bonus(item.name(), query)
}

/// [`EXACT_NAME`] or [`NAME_PREFIX`] for `name` and the lowercase `query`, else 0.
fn name_bonus(name: &str, query: &str) -> i32 {
    let (exact, prefix) = if name.is_ascii() && query.is_ascii() {
        (
            name.eq_ignore_ascii_case(query),
            name.len() >= query.len()
                && name.as_bytes()[..query.len()].eq_ignore_ascii_case(query.as_bytes()),
        )
    } else {
        let name = name.to_lowercase();
        (name == query, name.starts_with(query))
    };
    if exact {
        EXACT_NAME
    } else if prefix {
        NAME_PREFIX
    } else {
        0
    }
}

/// Whether `kind` is Events, in the core group or `events.k8s.io`.
fn is_event_kind(kind: &KindId) -> bool {
    kind.plural() == "events" && matches!(kind.group(), "" | "events.k8s.io")
}

/// Finds the lowercase words of a query in texts, in order, each as a run of characters. A run
/// may step over a separator the word does not have, so "webapi" is a run in `web-api`.
///
/// Built once per query. Every partial run is tracked at once, one bit per word position, so a
/// text is read once however often it repeats the word's start.
#[derive(Debug)]
struct RunFinder {
    words: Vec<RunWord>,
    /// Reused between texts: bit `i` is set when a run has matched the word's first `i + 1`
    /// characters and is still alive at the character just read.
    partial: Vec<u64>,
}

/// One word of a [`RunFinder`].
#[derive(Debug)]
struct RunWord {
    len: usize,
    /// The word's distinct characters, sorted, so the ASCII ones come first.
    chars: Vec<char>,
    /// For each ASCII character, its index in `chars`, or [`NOT_IN_WORD`].
    ascii: [u8; 128],
    /// For each of `chars`, a bitset of the positions holding it, `len.div_ceil(64)` values.
    positions: Vec<u64>,
}

/// In [`RunWord::ascii`]: the word does not have this character.
const NOT_IN_WORD: u8 = u8::MAX;

impl RunWord {
    fn new(word: &str) -> Self {
        let word: Vec<char> = word.chars().collect();
        let mut chars = word.clone();
        chars.sort_unstable();
        chars.dedup();
        let blocks = word.len().div_ceil(64);
        let mut positions = vec![0; chars.len() * blocks];
        for (at, c) in word.iter().enumerate() {
            if let Ok(ix) = chars.binary_search(c) {
                positions[ix * blocks + at / 64] |= 1 << (at % 64);
            }
        }
        let mut ascii = [NOT_IN_WORD; 128];
        for (ix, c) in chars.iter().enumerate().take_while(|(_, c)| c.is_ascii()) {
            ascii[*c as usize] = ix as u8;
        }
        RunWord {
            len: word.len(),
            chars,
            ascii,
            positions,
        }
    }

    /// Takes in one character, `ix` in `chars` if the word has it. Returns whether a run of the
    /// whole word ends here, the earliest any run can.
    #[inline]
    fn step(&self, ix: Option<usize>, separator: bool, partial: &mut [u64]) -> bool {
        let Some(ix) = ix else {
            // A separator the word does not have leaves every run where it is.
            if !separator {
                partial.fill(0);
            }
            return false;
        };
        let blocks = partial.len();
        let here = &self.positions[ix * blocks..(ix + 1) * blocks];
        let mut carry = 1;
        for b in 0..blocks {
            let old = partial[b];
            // The runs whose next character is this one advance. Any other run steps over a
            // separator, and ends on anything else.
            let above = if b + 1 < blocks { here[b + 1] << 63 } else { 0 };
            let stay = if separator {
                old & !((here[b] >> 1) | above)
            } else {
                0
            };
            partial[b] = (((old << 1) | carry) & here[b]) | stay;
            carry = old >> 63;
        }
        partial[blocks - 1] & (1 << ((self.len - 1) % 64)) != 0
    }

    /// Where the first run of this word in the ASCII `text` ends.
    fn end_in_ascii(&self, text: &[u8], partial: &mut Vec<u64>) -> Option<usize> {
        partial.clear();
        partial.resize(self.len.div_ceil(64), 0);
        let mut at = 0;
        // A plain loop: this runs for every object per keystroke.
        while at < text.len() {
            let byte = text[at].to_ascii_lowercase();
            at += 1;
            let ix = self.ascii[byte as usize];
            let ix = (ix != NOT_IN_WORD).then_some(ix as usize);
            if self.step(ix, is_separator(byte as char), partial) {
                return Some(at);
            }
        }
        None
    }

    /// Reads `text` up to the end of the first run of this word. Returns whether there is one.
    fn find(&self, text: &mut impl Iterator<Item = char>, partial: &mut Vec<u64>) -> bool {
        partial.clear();
        partial.resize(self.len.div_ceil(64), 0);
        text.any(|c| {
            let ix = self.chars.binary_search(&c).ok();
            self.step(ix, is_separator(c), partial)
        })
    }
}

impl RunFinder {
    fn new<'a>(words: impl IntoIterator<Item = &'a str>) -> Self {
        RunFinder {
            words: words.into_iter().map(RunWord::new).collect(),
            partial: Vec::new(),
        }
    }

    /// Whether the words appear in `text` in order, each as a run.
    fn found_in(&mut self, text: &str) -> bool {
        let partial = &mut self.partial;
        if text.is_ascii() {
            let mut rest = text.as_bytes();
            self.words.iter().all(|w| {
                let end = w.end_in_ascii(rest, partial);
                end.map(|end| rest = &rest[end..]).is_some()
            })
        } else {
            let mut text = text.chars().flat_map(char::to_lowercase);
            self.words.iter().all(|w| w.find(&mut text, partial))
        }
    }
}

fn is_separator(c: char) -> bool {
    matches!(c, '-' | '_' | '.' | '/' | ' ')
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
                event: false,
                name_at: 0,
            });
            index.kinds.push(Kind {
                id: entry.id.clone(),
                label: entry.label.clone(),
                in_memory: None,
            });
        }
        for snapshot in snapshots {
            let Some(&kind) = by_id.get(&snapshot.kind) else {
                continue;
            };
            let label = &index.kinds[kind as usize].label;
            let event = is_event_kind(&snapshot.kind);
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
                    event,
                    name_at: row.namespace.as_ref().map_or(0, |ns| ns.len() as u32 + 1),
                    text,
                });
                index.objects += 1;
            }
            let kind = &mut index.kinds[kind as usize];
            kind.in_memory = Some(kind.in_memory.unwrap_or(0) + snapshot.rows.len());
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
    /// index order. With one: by score, so a closer match always wins. The name counts most: the
    /// query being the whole name, then its start, then found in the name alone (not across the
    /// namespace) earns extra points. A match of the text beats the same match of the second
    /// text; among equal scores, the shorter text first, then index order (kinds first).
    ///
    /// When any object has the query as a run, in its text or second text, objects of the same
    /// sort with only scattered letters are left out: Events and other objects are judged
    /// apart, so a run in an Event hides no other object. Kinds always stay: "svc" still finds
    /// Services. Event objects come after every other match.
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
        let lowered: String = query
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        let mut matched: Vec<u32> = Vec::new();
        // (points, item, whether the text itself matched)
        let mut scored: Vec<(i32, u32, bool)> = Vec::new();
        // Ranked by the quick score; only the best RESULTS get the full scoring below.
        let mut consider = |ix: u32, matcher: &mut Matcher| {
            let item = &index.items[ix as usize];
            let found = match matcher.quick_match(&item.text, item.ascii) {
                Some((points, start)) => Some((
                    points + name_points(matcher, item, &lowered, start, false),
                    true,
                )),
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
        // One typed character is always a run.
        if lowered.chars().nth(1).is_some() {
            let words: Vec<String> = query
                .split_whitespace()
                .map(|w| w.chars().flat_map(char::to_lowercase).collect())
                .collect();
            let mut finder = RunFinder::new(words.iter().map(String::as_str));
            let runs: Vec<bool> = scored
                .iter()
                .map(|s| {
                    let item = &index.items[s.1 as usize];
                    // An object's second text ends with its text, so one read covers both.
                    // A kind is never filtered, so its runs do not matter.
                    item.object.is_some() && finder.found_in(&item.alt)
                })
                .collect();
            let run_among = |event: bool| {
                scored.iter().zip(&runs).any(|(s, &run)| {
                    let item = &index.items[s.1 as usize];
                    run && item.object.is_some() && item.event == event
                })
            };
            let (object_run, event_run) = (run_among(false), run_among(true));
            let mut runs = runs.iter();
            scored.retain(|s| {
                let run = runs.next().is_some_and(|&run| run);
                let item = &index.items[s.1 as usize];
                let run_exists = if item.event { event_run } else { object_run };
                run || item.object.is_none() || !run_exists
            });
        }
        let total = scored.len();
        let length = |ix: u32| index.items[ix as usize].text.len();
        let rank = |points: i32, ix: u32| {
            let event = index.items[ix as usize].event;
            (event, std::cmp::Reverse(points), length(ix), ix)
        };
        let order = |a: &(i32, u32, bool)| rank(a.0, a.1);
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
                    let bonus = name_points(&mut matcher, item, &lowered, 0, true);
                    matcher
                        .score(&item.text)
                        .map_or((quick, Vec::new()), |s| (s.points + bonus, s.ranges))
                } else {
                    let points = matcher.points(&item.alt).map_or(quick, |p| p - ALT_PENALTY);
                    (points, Vec::new())
                };
                (points, Hit { item: ix, ranges })
            })
            .collect();
        listed.sort_unstable_by_key(|(points, hit)| rank(*points, hit.item));
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

    use super::{Index, RESULTS, RunFinder, Search, is_separator};

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
        assert_eq!(index.kinds[0].in_memory, Some(2));
        assert_eq!(index.kinds[2].in_memory, Some(1));
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
        // The whole name first, then the name it starts.
        assert_eq!(
            texts(&index, &mut search, "web"),
            ["demo/web", "demo/web-1"]
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

    // covers UP-31: the name decides before the namespace and the kind: the Deployment named
    // web before the ConfigMap web-cfg; equal matches the shorter first.
    #[test]
    fn the_exact_name_ranks_first() {
        let index = Index::new(
            &[
                entry("", "configmaps", "ConfigMap", "ConfigMaps"),
                entry("apps", "deployments", "Deployment", "Deployments"),
                entry("", "pods", "Pod", "Pods"),
            ],
            &[
                snapshot(
                    KindId::new("", "configmaps"),
                    &[(Some("demo"), "web-cfg"), (Some("demo"), "a-web")],
                ),
                snapshot(KindId::new("apps", "deployments"), &[(Some("demo"), "web")]),
                snapshot(
                    KindId::pods(),
                    &[(Some("x"), "api-10"), (Some("x"), "api-1")],
                ),
            ],
        );
        let mut search = Search::default();
        assert_eq!(
            texts(&index, &mut search, "web"),
            ["demo/web", "demo/web-cfg", "demo/a-web"]
        );
        assert_eq!(texts(&index, &mut search, "ap1"), ["x/api-1", "x/api-10"]);
        assert_eq!(index.items[3].name(), "web-cfg");
        assert_eq!(index.items[0].name(), "ConfigMaps");
    }

    // covers UP-31: a cluster-scoped object and a kind earn the name's points as a namespaced
    // object does: with the same match, the shorter text first.
    #[test]
    fn the_name_counts_the_same_with_or_without_a_namespace() {
        let index = Index::new(
            &[
                entry("", "nodes", "Node", "Nodes"),
                entry("", "pods", "Pod", "Pods"),
            ],
            &[
                snapshot(KindId::pods(), &[(Some("x"), "work-1")]),
                snapshot(KindId::new("", "nodes"), &[(None, "work-1")]),
            ],
        );
        let mut search = Search::default();
        assert_eq!(texts(&index, &mut search, "work"), ["work-1", "x/work-1"]);
        assert_eq!(texts(&index, &mut search, "wk1"), ["work-1", "x/work-1"]);
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
        // The exact name first, then the shorter names it starts.
        assert_eq!(&*index.items[found.hits[0].item as usize].text, "demo/api");
        assert_eq!(
            &*index.items[found.hits[1].item as usize].text,
            "demo/api-0"
        );
        let gapped = search.run(&index, "ap1");
        assert!(gapped.matched > 0);
        let empty = search.run(&index, "");
        assert_eq!(empty.hits.len(), RESULTS);
        assert_eq!(empty.matched, RESULTS + 52);
    }

    // #418: Event objects follow every other match, even a weaker one, and a crowd of them
    // cannot push another object out of the listed results.
    #[test]
    fn events_come_after_other_matches() {
        let index = Index::new(
            &[
                entry("", "pods", "Pod", "Pods"),
                entry("", "events", "Event", "Events"),
            ],
            &[
                snapshot(
                    KindId::events(),
                    &[
                        (Some("demo"), "web-1.17a"),
                        (Some("demo"), "web-1.17b"),
                        (Some("demo"), "web-1.17c"),
                    ],
                ),
                snapshot(
                    KindId::pods(),
                    &[
                        (Some("demo"), "my-long-prefix-web-1"),
                        (Some("demo"), "w-e-b-x-1"),
                    ],
                ),
            ],
        );
        assert_eq!(
            texts(&index, &mut Search::default(), "web-1"),
            [
                "demo/my-long-prefix-web-1",
                "demo/web-1.17a",
                "demo/web-1.17b",
                "demo/web-1.17c",
            ]
        );
        // The kind is not demoted, and Events alone are still found.
        assert_eq!(texts(&index, &mut Search::default(), "events")[0], "Events");
        assert_eq!(
            texts(&index, &mut Search::default(), "events 17b"),
            ["demo/web-1.17b"]
        );

        let names: Vec<String> = (0..RESULTS).map(|i| format!("web-{i}.a")).collect();
        let rows: Vec<(Option<&str>, &str)> =
            names.iter().map(|n| (Some("demo"), n.as_str())).collect();
        let crowded = Index::new(
            &[
                entry("", "events", "Event", "Events"),
                entry("", "pods", "Pod", "Pods"),
            ],
            &[
                snapshot(KindId::events(), &rows),
                snapshot(KindId::pods(), &[(Some("a-long-namespace"), "the-web-pod")]),
            ],
        );
        let found = Search::default().run(&crowded, "web");
        assert_eq!(found.matched, RESULTS + 1);
        assert_eq!(
            &*crowded.items[found.hits[0].item as usize].text,
            "a-long-namespace/the-web-pod"
        );
    }

    // #418: scattered letters are dropped when a run exists, kept when none does, and come
    // back when the query grows past every run.
    #[test]
    fn scattered_matches_give_way_to_runs() {
        let index = Index::new(
            &[
                entry("", "pods", "Pod", "Pods"),
                entry("", "services", "Service", "Services"),
            ],
            &[snapshot(
                KindId::pods(),
                &[
                    (Some("demo"), "web-api"),
                    (Some("demo"), "wide-eyed-bot"),
                    (Some("demo"), "my-svc"),
                ],
            )],
        );
        let mut search = Search::default();
        assert_eq!(texts(&index, &mut search, "web"), ["demo/web-api"]);
        assert_eq!(
            search.run(&index, "web").matched,
            1,
            "the count leaves out what was dropped"
        );
        // A run across a separator counts, and the query may have spaces.
        assert_eq!(texts(&index, &mut search, "webapi"), ["demo/web-api"]);
        assert_eq!(
            texts(&index, &mut Search::default(), "pods web"),
            ["demo/web-api"]
        );
        // "we" hides the scattered object; typing on past every run brings it back.
        let mut grown = Search::default();
        assert_eq!(texts(&index, &mut grown, "we"), ["demo/web-api"]);
        assert_eq!(texts(&index, &mut grown, "wedb"), ["demo/wide-eyed-bot"]);
        // A kind stays even when only its letters match.
        assert_eq!(
            texts(&index, &mut Search::default(), "svc"),
            ["demo/my-svc", "Services"]
        );
    }

    // #418: a run in an Event does not hide an ordinary object, and the two are filtered apart:
    // scattered Events still give way to an Event with a run, and ordinary objects to theirs.
    #[test]
    fn events_and_other_objects_are_filtered_apart() {
        let entries = [
            entry("", "pods", "Pod", "Pods"),
            entry("", "events", "Event", "Events"),
        ];
        let index = Index::new(
            &entries,
            &[
                snapshot(KindId::pods(), &[(Some("demo"), "api-1")]),
                snapshot(
                    KindId::events(),
                    &[(Some("demo"), "web.17a1"), (Some("demo"), "api-1.17b")],
                ),
            ],
        );
        assert_eq!(
            texts(&index, &mut Search::default(), "a1"),
            ["demo/api-1", "demo/web.17a1"]
        );

        let index = Index::new(
            &entries,
            &[
                snapshot(
                    KindId::pods(),
                    &[(Some("demo"), "api-1"), (Some("demo"), "a1-web")],
                ),
                snapshot(KindId::events(), &[(Some("demo"), "api-1.17b")]),
            ],
        );
        assert_eq!(
            texts(&index, &mut Search::default(), "a1"),
            ["demo/a1-web", "demo/api-1.17b"]
        );
    }

    /// The rule [`RunFinder`] implements, one start at a time.
    fn plain_run_end(text: &[char], word: &[char]) -> Option<usize> {
        (0..text.len()).find_map(|start| {
            if text[start] != *word.first()? {
                return None;
            }
            let (mut t, mut w) = (start, 0);
            while w < word.len() && t < text.len() {
                if text[t] == word[w] {
                    w += 1;
                } else if !is_separator(text[t]) {
                    return None;
                }
                t += 1;
            }
            (w == word.len()).then_some(t)
        })
    }

    fn plain_has_run(text: &str, words: &[&str]) -> bool {
        let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
        let mut rest = text.as_slice();
        words.iter().all(|word| {
            let word: Vec<char> = word.chars().collect();
            let end = plain_run_end(rest, &word);
            end.map(|end| rest = &rest[end..]).is_some()
        })
    }

    /// Every string of up to `max` characters from `alphabet`.
    fn strings(alphabet: &[char], max: usize) -> Vec<String> {
        let mut all = vec![String::new()];
        let mut from = 0;
        for _ in 0..max {
            let to = all.len();
            for i in from..to {
                for c in alphabet {
                    let next = format!("{}{c}", all[i]);
                    all.push(next);
                }
            }
            from = to;
        }
        all
    }

    // #418: the one-pass check agrees with the plain rule on every short text and word, over
    // repeats, separators in the text and in the word, several words and non-ASCII.
    #[test]
    fn the_run_check_keeps_the_rule() {
        for alphabet in [['a', 'b', '-'], ['é', 'b', '.']] {
            let texts = strings(&alphabet, 7);
            let words: Vec<String> = strings(&alphabet, 3).split_off(1);
            for word in &words {
                let mut finder = RunFinder::new([word.as_str()]);
                for text in &texts {
                    let expected = plain_has_run(text, &[word]);
                    assert_eq!(finder.found_in(text), expected, "{word:?} in {text:?}");
                }
            }
            for (first, second) in words.iter().zip(words.iter().rev()) {
                let mut finder = RunFinder::new([first.as_str(), second.as_str()]);
                for text in &texts {
                    let expected = plain_has_run(text, &[first, second]);
                    assert_eq!(
                        finder.found_in(text),
                        expected,
                        "{first:?} {second:?} {text:?}"
                    );
                }
            }
        }
        // Case, and a word longer than one block of bits, with a separator across the boundary.
        let long = "a".repeat(70);
        let mut finder = RunFinder::new([long.as_str()]);
        assert!(finder.found_in(&format!("{}-{}", "A".repeat(64), "a".repeat(6))));
        assert!(!finder.found_in(&format!("{}x{}", "a".repeat(64), "a".repeat(6))));
        assert!(!finder.found_in(&"a".repeat(69)));
    }

    // #418: a long repeated prefix that never completes a run costs one pass per name, not one
    // per occurrence of the query's first letter. These names are over twice as long as the
    // ones below and every one matches, so the linear passes alone take a few hundred ms in
    // this unoptimized build. Rescanning the prefix took over 3 s, which the limit still fails.
    #[test]
    fn a_repeated_prefix_over_fifty_thousand_objects_fits_in_a_frame() {
        let prefix = "a".repeat(60);
        let names: Vec<String> = (0..50_000).map(|i| format!("{prefix}x-b-{i}")).collect();
        let rows: Vec<(Option<&str>, &str)> =
            names.iter().map(|n| (Some("demo"), n.as_str())).collect();
        let index = Index::new(
            &[entry("", "pods", "Pod", "Pods")],
            &[snapshot(KindId::pods(), &rows)],
        );
        let query = format!("{prefix}b");
        let mut spent = Duration::MAX;
        for _ in 0..3 {
            let start = Instant::now();
            let found = Search::default().run(&index, &query);
            spent = spent.min(start.elapsed());
            assert_eq!(found.matched, 50_000);
        }
        assert!(spent < Duration::from_secs(1), "{spent:?}");
    }

    /// `objects` objects across 40 kinds and 200 namespaces, named as real clusters name them.
    fn large(objects: usize) -> Index {
        let per_kind = objects / 40;
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
                let names: Vec<(String, String)> = (0..per_kind)
                    .map(|i| {
                        let n = k * per_kind + i;
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

    /// Counts the allocations made on the current thread, so other tests running at once do
    /// not change the count.
    mod allocations {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static COUNT: Cell<u64> = const { Cell::new(0) };
        }

        struct Counting;

        fn count() {
            let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        }

        // SAFETY: every call is passed unchanged to the system allocator.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                count();
                unsafe { System.alloc(layout) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                count();
                unsafe { System.alloc_zeroed(layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                count();
                unsafe { System.realloc(ptr, layout, new_size) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                unsafe { System.dealloc(ptr, layout) }
            }
        }

        #[global_allocator]
        static ALLOCATOR: Counting = Counting;

        /// What `f` returns, and how many allocations it made.
        pub fn counted<T>(f: impl FnOnce() -> T) -> (T, u64) {
            let before = COUNT.with(Cell::get);
            let value = f();
            (value, COUNT.with(Cell::get) - before)
        }
    }

    /// A name typed letter by letter, the switcher's slowest ordinary use.
    const TYPED: &str = "team-17/payments-api-00bc9";

    // covers UP-31: a keystroke allocates for the results it lists, never per object searched.
    // The count does not depend on how busy the machine is.
    #[test]
    fn typing_over_fifty_thousand_objects_allocates_per_result() {
        let index = large(50_000);
        assert_eq!(index.objects, 50_000);
        let mut search = Search::default();
        let mut most = 0;
        for end in 1..=TYPED.len() {
            let (found, made) = allocations::counted(|| search.run(&index, &TYPED[..end]));
            assert!(!found.hits.is_empty(), "{}", &TYPED[..end]);
            most = most.max(made);
        }
        let found = search.run(&index, TYPED);
        assert_eq!(
            &*index.items[found.hits[0].item as usize].text,
            "team-17/payments-api-00bc9-0"
        );
        // 462 here: a few for each listed result. One per object would be at least 50,000.
        assert!(most <= RESULTS as u64 * 4, "{most} allocations");
    }

    /// CPU time spent on this thread, excluding time when it was descheduled.
    fn thread_cpu_time() -> Duration {
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: clock_gettime initializes the writable timespec on success.
        let time = unsafe {
            assert_eq!(
                libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()),
                0
            );
            time.assume_init()
        };
        Duration::new(
            time.tv_sec.try_into().unwrap(),
            time.tv_nsec.try_into().unwrap(),
        )
    }

    fn typing_cpu_time(index: &Index) -> Duration {
        let mut search = Search::default();
        let start = thread_cpu_time();
        for end in 1..=TYPED.len() {
            std::hint::black_box(search.run(index, &TYPED[..end]));
        }
        thread_cpu_time() - start
    }

    // covers UP-31: eight times the objects should take about eight times the work, with
    // headroom for cache effects. Quadratic work grows by a factor of 64.
    #[test]
    fn typing_time_grows_linearly_with_the_objects() {
        const LIMIT: u32 = 12;
        let (small, big) = (large(4_000), large(32_000));
        let mut small_samples = [Duration::ZERO; 3];
        let mut big_samples = [Duration::ZERO; 3];
        for (small_sample, big_sample) in small_samples.iter_mut().zip(&mut big_samples) {
            *small_sample = typing_cpu_time(&small);
            *big_sample = typing_cpu_time(&big);
        }
        small_samples.sort_unstable();
        big_samples.sort_unstable();
        assert!(
            big_samples[1] < small_samples[1] * LIMIT,
            "typing CPU times: 4,000 objects {small_samples:?}, 32,000 objects {big_samples:?}"
        );
    }
}
