//! Per-kind row stores and the immutable snapshots they publish (ADR-0002).
//!
//! A store is owned by its kind's watch task, so it needs no lock. Front ends only ever see
//! [`KindSnapshot`]s: shared, read-only values swapped through a `tokio::sync::watch` channel at
//! most about every 100 ms.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cluster::ClusterId;
use crate::read::KindId;
use crate::table::{self, Column, Interner, Layout, ListFormat, Row, WireObject};

/// Where a kind's list stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SyncState {
    /// Not watched: the cluster is not active, or (in On view mode) nothing has the kind open.
    /// Rows are empty.
    #[default]
    Idle,
    /// The first list is loading. Rows are empty.
    Loading,
    /// Rows are current and live updates are flowing (or, for polled kinds, the last poll worked).
    Synced,
    /// The watch broke and is being re-established, or a relist is running. Rows are the last
    /// known state and may be stale: show it.
    Reconnecting { error: Option<String> },
    /// The server refuses the list (forbidden, not found). Retried slowly. Rows may be empty.
    Failed { error: String },
    /// Discovery does not know this kind on this cluster.
    Unavailable,
}

impl SyncState {
    /// Whether the rows can be shown as current.
    pub fn is_current(&self) -> bool {
        matches!(self, SyncState::Synced)
    }
}

/// How a kind is kept up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Feed {
    /// List, then watch.
    #[default]
    Watch,
    /// Listed again on an interval. For APIs that cannot watch (metrics.k8s.io).
    Poll,
}

/// An immutable view of one kind's list on one cluster.
#[derive(Debug, Clone, Default)]
pub struct KindSnapshot {
    pub cluster: ClusterId,
    pub kind: KindId,
    pub namespaced: bool,
    /// Shared by every snapshot until the server's column definitions change.
    pub columns: Arc<[Column]>,
    /// Sorted by namespace, then name.
    pub rows: Vec<Arc<Row>>,
    pub state: SyncState,
    pub format: ListFormat,
    pub feed: Feed,
    /// Increases with every snapshot published, and never repeats within the process: cache
    /// derived data (such as [`crate::CoreHandle::compute_view`] results) by it.
    pub version: u64,
    /// The rows' revision: it moves only when a list completes (the first list, a relist) or
    /// a row changes, never when the same rows are published again, and never repeats within
    /// the process. 0 for a snapshot no watch built. Tells rows listed anew from a republish.
    pub revision: u64,
    /// Set when the user may not list this kind in all namespaces, so it is listed namespace
    /// by namespace instead: the rows come only from these namespaces.
    pub limited_to: Option<Arc<NamespaceLimit>>,
}

/// Which namespaces a namespace-limited list covers (see [`KindSnapshot::limited_to`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamespaceLimit {
    /// Namespaces listed, sorted.
    pub listed: Vec<String>,
    /// Namespaces tried whose list was refused too, sorted.
    pub denied: Vec<String>,
    /// Why the list is limited: the server's answer to the all-namespaces list.
    pub reason: String,
    /// For each of `listed`, the [`KindSnapshot::revision`] of that namespace's rows merged
    /// in. A merged list can be published again from rows that did not change; these tell a
    /// namespace listed anew (or changed) from one that was not.
    pub revisions: Vec<u64>,
}

impl NamespaceLimit {
    /// The revision of `namespace`'s rows merged in, if it is listed.
    pub fn revision_of(&self, namespace: &str) -> Option<u64> {
        let at = self.listed.iter().position(|n| n == namespace)?;
        self.revisions.get(at).copied()
    }
}

/// Row revisions come from one counter for the whole process, so a revision never repeats,
/// even for a watch started again.
static REVISION: AtomicU64 = AtomicU64::new(0);

impl KindSnapshot {
    pub(crate) fn empty(cluster: ClusterId, kind: KindId, state: SyncState) -> Self {
        KindSnapshot {
            cluster,
            kind,
            state,
            columns: Arc::new([]),
            ..Default::default()
        }
    }

    /// The rows in one namespace, found by binary search (rows are sorted by namespace).
    pub fn namespace_range(&self, namespace: &str) -> Range<usize> {
        fn ns(r: &Arc<Row>) -> &str {
            r.namespace.as_deref().unwrap_or("")
        }
        let start = self.rows.partition_point(|r| ns(r) < namespace);
        let end = self.rows.partition_point(|r| ns(r) <= namespace);
        start..end
    }

    /// The rows in any of `namespaces`, in snapshot order. Cost is O(k log n) plus the rows
    /// returned, so it is cheap to call per frame.
    pub fn in_namespaces<'a, S: AsRef<str>>(
        &'a self,
        namespaces: &'a [S],
    ) -> impl Iterator<Item = &'a Arc<Row>> + 'a {
        let mut ranges: Vec<Range<usize>> = namespaces
            .iter()
            .map(|n| self.namespace_range(n.as_ref()))
            .collect();
        ranges.sort_by_key(|r| r.start);
        ranges.dedup();
        ranges.into_iter().flat_map(move |r| self.rows[r].iter())
    }

    /// Finds a row by uid. Linear; for occasional lookups (selection after an update).
    pub fn row_by_uid(&self, uid: &str) -> Option<&Arc<Row>> {
        self.rows.iter().find(|r| &*r.uid == uid)
    }

    /// Row names, for small kinds such as namespaces.
    pub fn names(&self) -> Vec<&str> {
        self.rows.iter().map(|r| &*r.name).collect()
    }

    /// Index of the column with `role`, if any.
    pub fn column(&self, role: table::ColumnRole) -> Option<usize> {
        self.columns.iter().position(|c| c.role == role)
    }
}

/// The rows of one kind, keyed by uid.
#[derive(Debug)]
pub(crate) struct Store {
    namespaced: bool,
    layout: Option<Layout>,
    rows: HashMap<Box<str>, Arc<Row>>,
    /// Rows of a list in progress; swapped in when it completes, so a relist never shows a
    /// half-loaded list.
    pending: Option<HashMap<Box<str>, Arc<Row>>>,
    /// The layout the pending rows were built with; it replaces `layout` with them, so a
    /// snapshot never pairs new columns with old cells.
    pending_layout: Option<Layout>,
    interner: Interner,
    /// The visible rows' revision ([`KindSnapshot::revision`]).
    revision: u64,
}

/// The interned strings a row uses: its namespace, label keys and values, and text cells.
fn row_strings(row: &Row) -> impl Iterator<Item = &Arc<str>> {
    row.namespace
        .iter()
        .chain(row.labels.iter().flat_map(|(k, v)| [k, v]))
        .chain(row.cells.iter().filter_map(|cell| match cell {
            crate::table::Cell::Text(s) => Some(s),
            _ => None,
        }))
}

/// What [`Store::apply`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    /// The visible rows changed.
    pub changed: bool,
    /// The column layout of the visible rows changed while rows built with the old one exist:
    /// the caller should relist.
    pub relayout: bool,
}

impl Store {
    pub fn new(namespaced: bool) -> Self {
        Store {
            namespaced,
            layout: None,
            rows: HashMap::new(),
            pending: None,
            pending_layout: None,
            interner: Interner::default(),
            revision: 0,
        }
    }

    /// The visible rows changed, or were listed anew.
    fn revise(&mut self) {
        self.revision = REVISION.fetch_add(1, Ordering::Relaxed) + 1;
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Starts a full list. Until [`Store::finish_resync`], changes go to a separate map.
    pub fn begin_resync(&mut self) {
        self.pending = Some(HashMap::new());
        self.pending_layout = self.layout.clone();
    }

    /// Replaces the rows with the completed list.
    pub fn finish_resync(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.rows = pending;
            if self.pending_layout.is_some() {
                self.layout = self.pending_layout.take();
            }
            // Snapshots published before (the hub's, a view's still on screen) hold the old
            // rows' strings, so a prune by reference count would keep them. Keep exactly what
            // the new rows use: the old strings go when their last holder lets go, whenever
            // that is, with no further events needed.
            self.interner
                .keep_only(self.rows.values().flat_map(|row| row_strings(row)));
            self.revise();
        }
    }

    /// Drops an unfinished list.
    pub fn abandon_resync(&mut self) {
        self.pending = None;
        self.pending_layout = None;
    }

    /// Applies an added, modified, listed, or (`delete`) deleted object. Rows collected by a
    /// resync are not visible until it finishes.
    ///
    /// When the object brings a different column layout while visible rows were built with the
    /// old one, nothing is applied and `relayout` is set: the caller relists, and the relist
    /// swaps the new layout in together with rows built for it. A snapshot never pairs new
    /// columns with old cells.
    pub fn apply(&mut self, object: WireObject, delete: bool, now: i64) -> Applied {
        let visible = self.pending.is_none();
        let slot = if visible {
            &mut self.layout
        } else {
            &mut self.pending_layout
        };
        if let Some(next) = table::next_layout(&object, self.namespaced, slot.as_ref()) {
            if visible && slot.is_some() && !self.rows.is_empty() {
                return Applied {
                    changed: false,
                    relayout: true,
                };
            }
            *slot = Some(next);
        }
        let rows = table::rows_with(object, slot.as_ref(), &mut self.interner, now);
        let target = self.pending.as_mut().unwrap_or(&mut self.rows);
        let mut changed = false;
        for row in rows {
            if delete {
                changed |= target.remove(&row.uid).is_some();
            } else {
                match target.get(&row.uid) {
                    Some(old) if **old == row => {}
                    _ => {
                        target.insert(row.uid.clone(), Arc::new(row));
                        changed = true;
                    }
                }
            }
        }
        if changed && visible {
            self.revise();
        }
        Applied {
            changed: changed && visible,
            relayout: false,
        }
    }

    /// Whether the server's column definitions (or the fallback layout) are known.
    pub fn has_layout(&self) -> bool {
        self.layout.is_some() || self.pending_layout.is_some()
    }

    /// Whether `object` is a Table with rows but no column definitions, while none are known
    /// yet: its cells cannot be mapped. Happens when a watch started on an empty list, whose
    /// first event (a bookmark) carried the definitions.
    pub fn lacks_columns_for(&self, object: &WireObject) -> bool {
        !self.has_layout()
            && object.is_table()
            && object.column_definitions.as_ref().is_none_or(Vec::is_empty)
            && object.rows.as_ref().is_some_and(|r| !r.is_empty())
    }

    /// Takes column definitions from an object without applying its rows (bookmarks, a probe
    /// list). Returns `true`, changing nothing, when they differ from the layout visible rows
    /// were built with: the caller relists (an edited CRD's printer columns can arrive in the
    /// first bookmark of a resumed watch).
    #[must_use]
    pub fn learn_columns(&mut self, object: &WireObject) -> bool {
        if !object.is_table() {
            return false;
        }
        let visible = self.pending.is_none();
        let slot = if visible {
            &mut self.layout
        } else {
            &mut self.pending_layout
        };
        let Some(next) = table::next_layout(object, self.namespaced, slot.as_ref()) else {
            return false;
        };
        if visible && slot.is_some() && !self.rows.is_empty() {
            return true;
        }
        *slot = Some(next);
        false
    }

    pub fn format(&self) -> ListFormat {
        self.layout.as_ref().map(|l| l.format).unwrap_or_default()
    }

    pub fn snapshot(&self, base: KindSnapshot) -> KindSnapshot {
        let mut rows: Vec<Arc<Row>> = self.rows.values().cloned().collect();
        rows.sort_unstable_by(|a, b| {
            (a.namespace.as_deref(), &*a.name).cmp(&(b.namespace.as_deref(), &*b.name))
        });
        let columns = match &self.layout {
            Some(l) => l.columns.clone(),
            None => Layout::fallback(self.namespaced).columns,
        };
        KindSnapshot {
            namespaced: self.namespaced,
            columns,
            rows,
            format: self.format(),
            revision: self.revision,
            ..base
        }
    }

    #[cfg(test)]
    pub fn apply_changed(&mut self, object: WireObject, delete: bool, now: i64) -> bool {
        self.apply(object, delete, now).changed
    }

    #[cfg(test)]
    pub fn get(&self, uid: &str) -> Option<&Arc<Row>> {
        self.rows.get(uid)
    }

    #[cfg(test)]
    pub fn interned(&self) -> usize {
        self.interner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::tests::pod_table;

    fn obj(rows: &[(&str, &str, &str, &str)], rv: &str) -> WireObject {
        serde_json::from_value(pod_table(rows, true, rv)).unwrap()
    }

    // covers UP-31: the revision moves when rows change or a list completes, and never when
    // the same rows are applied or published again.
    #[test]
    fn the_revision_moves_only_on_a_list_or_a_change() {
        let mut store = Store::new(true);
        let revision = |store: &Store| store.snapshot(KindSnapshot::default()).revision;
        assert_eq!(revision(&store), 0);
        store.apply_changed(obj(&[("u1", "a", "x", "Running")], "1"), false, 0);
        let first = revision(&store);
        assert!(first > 0);
        // The same row again, and publishing twice: no change.
        store.apply_changed(obj(&[("u1", "a", "x", "Running")], "1"), false, 0);
        assert_eq!(revision(&store), first);
        assert_eq!(revision(&store), first);
        // A changed row.
        store.apply_changed(obj(&[("u1", "a", "x", "Pending")], "2"), false, 0);
        let changed = revision(&store);
        assert!(changed > first);
        // A list in progress changes nothing visible; once complete, even with the same rows,
        // it is a list anew.
        store.begin_resync();
        store.apply_changed(obj(&[("u1", "a", "x", "Pending")], "3"), false, 0);
        assert_eq!(revision(&store), changed);
        store.finish_resync();
        assert!(revision(&store) > changed);
        // An abandoned list does not count.
        let listed = revision(&store);
        store.begin_resync();
        store.abandon_resync();
        assert_eq!(revision(&store), listed);
    }

    // covers UP-20
    #[test]
    fn apply_and_delete_by_uid() {
        let mut store = Store::new(true);
        assert!(store.apply_changed(obj(&[("u1", "b", "x", "Pending")], "1"), false, 0));
        assert!(store.apply_changed(obj(&[("u2", "a", "y", "Running")], "2"), false, 0));
        // Same content again: no change.
        assert!(!store.apply_changed(obj(&[("u2", "a", "y", "Running")], "2"), false, 0));
        assert!(store.apply_changed(obj(&[("u1", "b", "x", "Running")], "3"), false, 0));
        assert_eq!(store.get("u1").unwrap().cell_text(3, 0), "Running");
        let snap = store.snapshot(KindSnapshot::default());
        let names: Vec<&str> = snap.rows.iter().map(|r| &*r.name).collect();
        assert_eq!(names, ["y", "x"], "sorted by namespace then name");
        assert!(store.apply_changed(obj(&[("u1", "b", "x", "Running")], "4"), true, 0));
        assert!(!store.apply_changed(obj(&[("u1", "b", "x", "Running")], "4"), true, 0));
        assert_eq!(store.len(), 1);
    }

    // covers UP-20: hours of pod churn on a watch that never relists keep the interner bounded
    // (#257).
    #[test]
    fn churn_without_a_relist_keeps_the_interner_bounded() {
        let mut store = Store::new(true);
        for i in 0..50 {
            let uid = format!("live-{i}");
            let name = format!("web-{i}");
            let status = format!("live-status-{i}");
            store.apply(obj(&[(&uid, "demo", &name, &status)], "1"), false, 0);
        }
        let live = store.interned();
        // Each new pod has a unique status (as it would a unique IP), and is deleted again.
        for i in 0..40_000 {
            let uid = format!("u{i}");
            let name = format!("job-{i}");
            let status = format!("ip-10-0-{}-{}", i / 256, i % 256);
            let rv = i.to_string();
            store.apply(obj(&[(&uid, "demo", &name, &status)], &rv), false, 0);
            store.apply(obj(&[(&uid, "demo", &name, &status)], &rv), true, 0);
            assert!(
                store.interned() < 2 * live + 4096,
                "{} strings after {i} pods, {live} live",
                store.interned()
            );
        }
        assert_eq!(store.len(), 50);
    }

    // covers UP-20, UP-27: a relist that finds a big scale-down, while a view still holds the
    // old snapshot past the next publish, keeps none of the old strings once the view lets go,
    // with no further events (#257).
    #[test]
    fn relisted_strings_go_when_their_last_holder_lets_go() {
        let mut store = Store::new(true);
        for i in 0..5_000 {
            let uid = format!("old-{i}");
            let status = format!("old-status-{i}");
            store.apply(obj(&[(&uid, "demo", &uid, &status)], "1"), false, 0);
        }
        // A list on screen holds the snapshot from before the relist.
        let on_screen = store.snapshot(KindSnapshot::default());
        // One of the old rows' statuses (cells: name, namespace, ready, status).
        let old: Arc<str> = match &on_screen.rows[0].cells[3] {
            crate::table::Cell::Text(s) => s.clone(),
            other => panic!("a status cell, not {other:?}"),
        };
        assert!(old.starts_with("old-status-"), "{old}");
        store.begin_resync();
        store.apply(obj(&[("new", "demo", "new", "Running")], "2"), false, 0);
        store.finish_resync();
        // The store keeps only what its rows use, though the old strings are still held.
        assert!(store.interned() < 10, "{}", store.interned());
        // The hub publishes the new rows; the view keeps the old snapshot a while longer.
        let published = store.snapshot(KindSnapshot::default());
        assert_eq!(published.rows.len(), 1);
        assert_eq!(Arc::strong_count(&old), 2, "the view's row and this test");
        // The view lets go: nothing else holds the old string, with no event after it.
        drop(on_screen);
        assert_eq!(Arc::strong_count(&old), 1, "only this test");
    }

    // covers UP-20, UP-27: a burst after a relist, before the old snapshot is replaced, does
    // not keep the old strings or raise the next prune's threshold (#257).
    #[test]
    fn a_burst_before_the_next_publish_keeps_no_relisted_strings() {
        let mut store = Store::new(true);
        for i in 0..5_000 {
            let uid = format!("old-{i}");
            let status = format!("old-status-{i}");
            store.apply(obj(&[(&uid, "demo", &uid, &status)], "1"), false, 0);
        }
        let old = store.snapshot(KindSnapshot::default());
        store.begin_resync();
        store.apply(obj(&[("new", "demo", "new", "Running")], "2"), false, 0);
        store.finish_resync();
        // Pods come and go faster than the next publish.
        for i in 0..4_096 {
            let uid = format!("u{i}");
            let status = format!("s{i}");
            store.apply(obj(&[(&uid, "demo", &uid, &status)], "3"), false, 0);
            store.apply(obj(&[(&uid, "demo", &uid, &status)], "3"), true, 0);
        }
        let held = store.snapshot(KindSnapshot::default());
        drop(old);
        assert!(store.interned() < 100, "{}", store.interned());
        assert_eq!(held.rows.len(), 1);
        // The next automatic prune comes after PRUNE_MIN new strings, not after 5,000 more.
        for i in 0..4_096 {
            let uid = format!("v{i}");
            let status = format!("t{i}");
            store.apply(obj(&[(&uid, "demo", &uid, &status)], "4"), false, 0);
            store.apply(obj(&[(&uid, "demo", &uid, &status)], "4"), true, 0);
        }
        assert!(store.interned() < 100, "{}", store.interned());
    }

    // covers UP-27
    #[test]
    fn resync_swaps_only_when_complete() {
        let mut store = Store::new(true);
        store.apply(obj(&[("old", "a", "old", "Running")], "1"), false, 0);
        store.begin_resync();
        assert!(!store.apply_changed(obj(&[("new", "a", "new", "Running")], "5"), false, 0));
        assert!(
            store.get("old").is_some(),
            "old rows stay until the list completes"
        );
        store.finish_resync();
        assert!(store.get("old").is_none());
        assert!(store.get("new").is_some());
    }

    fn column_names(snap: &KindSnapshot) -> Vec<String> {
        snap.columns.iter().map(|c| c.name.to_string()).collect()
    }

    fn cells(snap: &KindSnapshot) -> Vec<Vec<String>> {
        snap.rows
            .iter()
            .map(|r| {
                (0..r.cells.len())
                    .map(|i| r.cell_text(i, 0).into_owned())
                    .collect()
            })
            .collect()
    }

    /// Every row has one cell per column, and the snapshot is the same before and after.
    fn assert_unchanged(store: &Store, before: &KindSnapshot) {
        let now = store.snapshot(KindSnapshot::default());
        assert!(Arc::ptr_eq(&now.columns, &before.columns), "columns kept");
        assert_eq!(column_names(&now), column_names(before));
        assert_eq!(cells(&now), cells(before), "cells kept");
        for row in &now.rows {
            assert_eq!(row.cells.len(), now.columns.len());
        }
    }

    // covers UP-22
    #[test]
    fn changed_columns_ask_for_a_relist_without_touching_the_visible_list() {
        let mut store = Store::new(true);
        store.apply(obj(&[("u1", "a", "x", "Running")], "1"), false, 0);
        let old = store.snapshot(KindSnapshot::default());
        // The same definitions again: nothing to do.
        let same = store.apply(obj(&[("u1", "a", "x", "Running")], "2"), false, 0);
        assert!(!same.relayout);

        // Different definitions on a live object: relist, and leave the visible rows, columns
        // and cells exactly as they were (the new object is not applied either).
        let mut changed = crate::table::tests::pod_table(&[("u2", "a", "y", "Running")], true, "3");
        changed["columnDefinitions"][1]["name"] = "Ready Now".into();
        changed["columnDefinitions"].as_array_mut().unwrap().pop();
        let applied = store.apply(serde_json::from_value(changed.clone()).unwrap(), false, 0);
        assert_eq!(
            applied,
            Applied {
                changed: false,
                relayout: true
            }
        );
        assert_unchanged(&store, &old);
        assert!(store.get("u2").is_none());

        // The same through a bookmark (column definitions without rows).
        let mut bookmark: WireObject = serde_json::from_value(changed.clone()).unwrap();
        bookmark.rows = Some(Vec::new());
        assert!(store.learn_columns(&bookmark));
        assert_unchanged(&store, &old);
        // Unchanged definitions in a bookmark are fine.
        let same: WireObject =
            serde_json::from_value(crate::table::tests::pod_table(&[], true, "4")).unwrap();
        assert!(!store.learn_columns(&same));

        // During the relist, the visible snapshot keeps the columns its rows were built with.
        store.begin_resync();
        let mut relisted = changed;
        relisted["columnDefinitions"][1]["name"] = "Ready Later".into();
        store.apply(serde_json::from_value(relisted).unwrap(), false, 0);
        assert_unchanged(&store, &old);
        store.finish_resync();
        let after = store.snapshot(KindSnapshot::default());
        assert!(column_names(&after).contains(&"Ready Later".to_owned()));
        assert!(!Arc::ptr_eq(&old.columns, &after.columns));
        for row in &after.rows {
            assert_eq!(row.cells.len(), after.columns.len());
        }
    }

    // covers UP-22
    #[test]
    fn an_empty_list_takes_new_columns_at_once() {
        let mut store = Store::new(true);
        let empty: WireObject =
            serde_json::from_value(crate::table::tests::pod_table(&[], true, "1")).unwrap();
        assert!(!store.learn_columns(&empty));
        assert!(store.has_layout());
        let mut changed = crate::table::tests::pod_table(&[], true, "2");
        changed["columnDefinitions"][1]["name"] = "Ready Now".into();
        let changed: WireObject = serde_json::from_value(changed).unwrap();
        assert!(!store.learn_columns(&changed), "no rows to rebuild");
        let snap = store.snapshot(KindSnapshot::default());
        assert!(column_names(&snap).contains(&"Ready Now".to_owned()));
    }

    // covers UP-21
    #[test]
    fn namespace_filter_uses_ranges() {
        let mut store = Store::new(true);
        for (i, ns) in ["a", "b", "b", "c", "d"].iter().enumerate() {
            let uid = format!("u{i}");
            let name = format!("n{i}");
            store.apply(obj(&[(&uid, ns, &name, "Running")], "1"), false, 0);
        }
        let snap = store.snapshot(KindSnapshot::default());
        assert_eq!(snap.namespace_range("b"), 1..3);
        assert_eq!(snap.namespace_range("zz"), 5..5);
        let picked: Vec<&str> = snap
            .in_namespaces(&["d", "b", "b"])
            .map(|r| &*r.name)
            .collect();
        assert_eq!(picked, ["n1", "n2", "n4"]);
    }
}
