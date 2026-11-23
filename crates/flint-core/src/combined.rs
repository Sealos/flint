//! One list over several clusters (ADR-0014): the members' snapshots of a kind, merged without
//! copying a row. Pure and read-only; run [`CombinedList::compute`] off the UI thread.
//!
//! - Columns are the union of the members' columns, by [`ColumnKey`], after a leading Cluster
//!   column. A cell whose cluster lacks the column is [`CombinedCell::Blank`].
//! - A row is addressed by [`RowRef`] within one list, and by [`CombinedRowId`] across lists. The
//!   id always holds the [`ClusterId`], so two clusters' objects of the same name never compare
//!   equal. The Cluster column shows, sorts and searches the display name passed in instead.
//! - A Secret's masked columns follow [`crate::redact::is_masked_column`] per member. Rows and
//!   snapshots stay private and out of `Debug`, so raw cells are not reachable from here, and
//!   sorting on a masked column keeps the default order.
//!
//! `compute` costs O(n log n) in the rows that pass the filter. The default order merges the
//! members' already sorted rows, and a collapsed group is counted but not sorted.

// module-tests: combined_tests.rs

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use crate::cluster::ClusterId;
use crate::read::KindId;
use crate::redact::{is_masked_column, is_secret_list};
use crate::settings::ClusterTag;
use crate::store::{KindSnapshot, SyncState};
use crate::table::{Cell, Column, ColumnRole, ColumnType, Row};
use crate::view::{ViewQuery, natural_cmp};

/// The index of the Cluster column in [`CombinedList::columns`].
pub const CLUSTER_COLUMN: usize = 0;

/// The Cluster column's [`ColumnKey::key`]. Server columns start with `server:`, so one headed
/// "Cluster" stays a separate column.
pub const CLUSTER_COLUMN_KEY: &str = "flint:cluster";

/// One member of a combined list.
#[derive(Clone)]
pub struct CombinedMember {
    pub snapshot: Arc<KindSnapshot>,
    /// The cluster's resolved name, as the sidebar shows it.
    pub display_name: String,
    /// The cluster's colour tag, drawn as a dot in the Cluster column.
    pub tag: Option<ClusterTag>,
}

impl fmt::Debug for CombinedMember {
    /// Leaves out the snapshot, whose rows hold a Secret's raw cells.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CombinedMember")
            .field("cluster", &self.snapshot.cluster)
            .field("display_name", &self.display_name)
            .field("tag", &self.tag)
            .field("rows", &self.snapshot.rows.len())
            .finish()
    }
}

/// Why members cannot be combined.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CombineError {
    #[error("a combined list holds one kind, got {first} and {other}")]
    MixedKinds { first: KindId, other: KindId },
    #[error("cluster {0} is in the combined list twice")]
    DuplicateCluster(ClusterId),
}

/// A column's identity across members: [`Column::key`], plus which of the columns with that key
/// it is within its member, since a server may repeat a header.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnKey {
    pub key: String,
    pub occurrence: u32,
}

/// One column of the union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombinedColumn {
    pub key: ColumnKey,
    pub name: Box<str>,
    /// [`ColumnType::Other`] when members disagree.
    pub ty: ColumnType,
    /// Empty when members disagree on the type.
    pub format: Box<str>,
    pub description: Box<str>,
    /// The lowest priority of any member, so a column that is default anywhere stays default.
    pub priority: i32,
    /// `None` for the Cluster column.
    pub role: Option<ColumnRole>,
}

/// Where a member finds a combined column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Absent,
    Masked,
    Shown(u32),
}

/// A member, in display-name order.
#[derive(Clone)]
pub struct MemberView {
    cluster: Arc<str>,
    display_name: Arc<str>,
    tag: Option<ClusterTag>,
    snapshot: Arc<KindSnapshot>,
    /// One per combined column.
    slots: Box<[Slot]>,
}

impl MemberView {
    /// The cluster's [`ClusterId`], for identity and exact filters.
    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    /// The name the Cluster column shows, sorts and searches.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn tag(&self) -> Option<ClusterTag> {
        self.tag
    }

    /// How current this member's rows are, for its note in the list.
    pub fn state(&self) -> &SyncState {
        &self.snapshot.state
    }

    /// How many rows this member holds, before any filter.
    pub fn row_count(&self) -> usize {
        self.snapshot.rows.len()
    }
}

impl fmt::Debug for MemberView {
    /// Leaves out the snapshot, whose rows hold a Secret's raw cells.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemberView")
            .field("cluster", &self.cluster)
            .field("display_name", &self.display_name)
            .field("tag", &self.tag)
            .field("rows", &self.row_count())
            .finish()
    }
}

/// A row's identity and metadata, without its cells: read those through
/// [`CombinedList::cell`], which masks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CombinedRowInfo<'a> {
    pub uid: &'a str,
    pub name: &'a str,
    pub namespace: Option<&'a str>,
    /// `metadata.creationTimestamp`, Unix seconds.
    pub created: Option<i64>,
}

/// A row's place in one [`CombinedList`]. Meaningless in any other list: keep a
/// [`CombinedRowId`] across updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowRef {
    member: u32,
    row: u32,
}

impl RowRef {
    /// The index into [`CombinedList::members`].
    pub fn member(self) -> usize {
        self.member as usize
    }
}

/// A row's identity across clusters and updates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CombinedRowId {
    pub cluster: Arc<str>,
    pub uid: Box<str>,
}

/// What a combined cell shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CombinedCell<'a> {
    /// The Cluster column: the display name, and the tag for its dot.
    Cluster {
        name: &'a str,
        tag: Option<ClusterTag>,
    },
    Value(Cow<'a, str>),
    /// The cluster lacks this column, or the cell is empty.
    Blank,
    /// A Secret's value: draw the mask.
    Masked,
}

/// Filter, sort and grouping for [`CombinedList::compute`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CombinedQuery {
    /// As [`ViewQuery::namespaces`].
    pub namespaces: Vec<String>,
    /// As [`ViewQuery::text`]; a cluster whose display name holds the text keeps all its rows.
    pub text: String,
    /// Only these clusters, matched exactly by id. Empty means all.
    pub clusters: Vec<ClusterId>,
    pub sort: Option<CombinedSort>,
    pub group_by_cluster: bool,
    /// Clusters, by id, whose group shows its header only.
    pub collapsed: Vec<ClusterId>,
}

/// A sort by column key, so it survives a change in the union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombinedSort {
    pub column: ColumnKey,
    pub descending: bool,
}

/// One cluster's section of a grouped view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CombinedGroup {
    /// The index into [`CombinedList::members`].
    pub member: usize,
    /// Rows that passed the filter, counted even while collapsed.
    pub count: usize,
    pub collapsed: bool,
    /// The header's line in the view.
    first_line: usize,
    /// The group's first entry in [`CombinedView::rows`].
    first_row: usize,
}

/// One line of a virtualized list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombinedLine {
    Header(CombinedGroup),
    Row(RowRef),
}

/// The rows to show, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CombinedView {
    rows: Vec<RowRef>,
    groups: Vec<CombinedGroup>,
    matched: usize,
}

impl CombinedView {
    /// The visible rows: those of a collapsed group are left out.
    pub fn rows(&self) -> &[RowRef] {
        &self.rows
    }

    /// One per selected cluster when grouped, including clusters with no match; else empty.
    pub fn groups(&self) -> &[CombinedGroup] {
        &self.groups
    }

    /// Rows that passed the filter, including those in collapsed groups.
    pub fn matched(&self) -> usize {
        self.matched
    }

    /// Lines to draw: the visible rows and any group headers.
    pub fn line_count(&self) -> usize {
        self.rows.len() + self.groups.len()
    }

    /// The line at `index`, in O(log groups).
    pub fn line(&self, index: usize) -> Option<CombinedLine> {
        if self.groups.is_empty() {
            return self.rows.get(index).copied().map(CombinedLine::Row);
        }
        if index >= self.line_count() {
            return None;
        }
        let after = self.groups.partition_point(|g| g.first_line <= index);
        let group = self.groups[after - 1];
        match index - group.first_line {
            0 => Some(CombinedLine::Header(group)),
            offset => self
                .rows
                .get(group.first_row + offset - 1)
                .copied()
                .map(CombinedLine::Row),
        }
    }
}

/// Several clusters' snapshots of one kind. Built only by [`CombinedList::new`], so the Cluster
/// column is always there, even with no members.
#[derive(Debug, Clone)]
pub struct CombinedList {
    columns: Arc<[CombinedColumn]>,
    members: Box<[MemberView]>,
}

impl CombinedList {
    /// Merges `members`. The result does not depend on their order: members are held by display
    /// name, then id.
    pub fn new(mut members: Vec<CombinedMember>) -> Result<Self, CombineError> {
        if let Some((first, rest)) = members.split_first()
            && let Some(other) = rest.iter().find(|m| m.snapshot.kind != first.snapshot.kind)
        {
            return Err(CombineError::MixedKinds {
                first: first.snapshot.kind.clone(),
                other: other.snapshot.kind.clone(),
            });
        }
        let mut seen = HashSet::with_capacity(members.len());
        for member in &members {
            if !seen.insert(member.snapshot.cluster.as_str()) {
                return Err(CombineError::DuplicateCluster(
                    member.snapshot.cluster.clone(),
                ));
            }
        }
        members.sort_by(|a, b| {
            natural_cmp(&a.display_name, &b.display_name)
                .then_with(|| a.display_name.cmp(&b.display_name))
                .then_with(|| a.snapshot.cluster.cmp(&b.snapshot.cluster))
        });

        let columns = union_columns(&members);
        let index: HashMap<&ColumnKey, usize> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (&c.key, i))
            .collect();
        let views: Vec<MemberView> = members
            .iter()
            .map(|member| {
                let snapshot = &member.snapshot;
                let secret = is_secret_list(snapshot.kind.as_str());
                let mut slots = vec![Slot::Absent; columns.len()];
                for (i, key) in column_keys(&snapshot.columns).iter().enumerate() {
                    slots[index[key]] = if is_masked_column(secret, &snapshot.columns, i) {
                        Slot::Masked
                    } else {
                        Slot::Shown(i as u32)
                    };
                }
                MemberView {
                    cluster: snapshot.cluster.as_str().into(),
                    display_name: member.display_name.as_str().into(),
                    tag: member.tag,
                    snapshot: snapshot.clone(),
                    slots: slots.into(),
                }
            })
            .collect();
        Ok(CombinedList {
            columns: columns.into(),
            members: views.into(),
        })
    }

    /// The Cluster column, then the union.
    pub fn columns(&self) -> &Arc<[CombinedColumn]> {
        &self.columns
    }

    /// The members, by display name.
    pub fn members(&self) -> &[MemberView] {
        &self.members
    }

    /// The column with this key.
    pub fn column_index(&self, key: &ColumnKey) -> Option<usize> {
        self.columns.iter().position(|c| c.key == *key)
    }

    /// The row's metadata. `at` must come from a view of this list.
    pub fn row_info(&self, at: RowRef) -> Option<CombinedRowInfo<'_>> {
        let row = self.checked_row(at)?;
        Some(CombinedRowInfo {
            uid: &row.uid,
            name: &row.name,
            namespace: row.namespace.as_deref(),
            created: row.created,
        })
    }

    fn checked_row(&self, at: RowRef) -> Option<&Row> {
        self.members
            .get(at.member())?
            .snapshot
            .rows
            .get(at.row as usize)
            .map(|row| &**row)
    }

    /// The row of member `member` with `uid`, whatever filters hide it. Linear.
    pub fn find_row(&self, member: usize, uid: &str) -> Option<&Row> {
        let rows = &self.members.get(member)?.snapshot.rows;
        rows.iter().find(|row| &*row.uid == uid).map(|row| &**row)
    }

    /// The identity of the row at `at`.
    pub fn row_id(&self, at: RowRef) -> Option<CombinedRowId> {
        let row = self.checked_row(at)?;
        Some(CombinedRowId {
            cluster: self.members[at.member()].cluster.clone(),
            uid: row.uid.clone(),
        })
    }

    /// What the cell shows. `now` is Unix seconds.
    pub fn cell(&self, at: RowRef, column: usize, now: i64) -> CombinedCell<'_> {
        let Some(member) = self.members.get(at.member()) else {
            return CombinedCell::Blank;
        };
        if column == CLUSTER_COLUMN {
            return CombinedCell::Cluster {
                name: &member.display_name,
                tag: member.tag,
            };
        }
        match member.slots.get(column) {
            None | Some(Slot::Absent) => CombinedCell::Blank,
            Some(Slot::Masked) => CombinedCell::Masked,
            Some(Slot::Shown(index)) => {
                let Some(row) = member.snapshot.rows.get(at.row as usize) else {
                    return CombinedCell::Blank;
                };
                match row.cell_text(*index as usize, now) {
                    text if text.is_empty() => CombinedCell::Blank,
                    text => CombinedCell::Value(text),
                }
            }
        }
    }

    /// The rows that pass `query`, in display order. Rows that tie keep the default order:
    /// namespace, name, then cluster. Blank cells sort last in both directions, so one cluster
    /// lacking a column never pushes the others' rows down.
    pub fn compute(&self, query: &CombinedQuery, now: i64) -> CombinedView {
        self.compute_where(query, now, |_, _| true)
    }

    /// [`Self::compute`] over the rows `keep` accepts, by member index. Counts and groups cover
    /// only those rows.
    pub fn compute_where(
        &self,
        query: &CombinedQuery,
        now: i64,
        keep: impl Fn(usize, &Row) -> bool,
    ) -> CombinedView {
        let sort = self.resolve_sort(query);
        let mut picked: Vec<(usize, Vec<u32>)> = (0..self.members.len())
            .filter(|&m| {
                query.clusters.is_empty()
                    || query
                        .clusters
                        .iter()
                        .any(|c| c.as_str() == &*self.members[m].cluster)
            })
            .map(|m| {
                let mut rows = self.filter_member(m, query, now);
                let snapshot = &self.members[m].snapshot;
                rows.retain(|&row| keep(m, &snapshot.rows[row as usize]));
                (m, rows)
            })
            .collect();
        let matched = picked.iter().map(|(_, rows)| rows.len()).sum();

        if !query.group_by_cluster {
            let mut rows = Vec::with_capacity(matched);
            for (m, indexes) in &picked {
                rows.extend(indexes.iter().map(|&row| RowRef {
                    member: *m as u32,
                    row,
                }));
            }
            self.order(&mut rows, sort, now);
            return CombinedView {
                rows,
                groups: Vec::new(),
                matched,
            };
        }

        if sort.is_some_and(|s| s.column == CLUSTER_COLUMN && s.descending) {
            picked.reverse();
        }
        let mut view = CombinedView {
            matched,
            ..CombinedView::default()
        };
        for (m, indexes) in picked {
            let collapsed = query
                .collapsed
                .iter()
                .any(|c| c.as_str() == &*self.members[m].cluster);
            view.groups.push(CombinedGroup {
                member: m,
                count: indexes.len(),
                collapsed,
                first_line: view.rows.len() + view.groups.len(),
                first_row: view.rows.len(),
            });
            if !collapsed {
                let mut rows: Vec<RowRef> = indexes
                    .into_iter()
                    .map(|row| RowRef {
                        member: m as u32,
                        row,
                    })
                    .collect();
                self.order(&mut rows, sort, now);
                view.rows.append(&mut rows);
            }
        }
        view
    }

    /// The sort to apply, or `None` when its column is gone or any member masks it.
    fn resolve_sort(&self, query: &CombinedQuery) -> Option<ResolvedSort> {
        let sort = query.sort.as_ref()?;
        let column = self.column_index(&sort.column)?;
        let masked = self.members.iter().any(|m| m.slots[column] == Slot::Masked);
        (!masked).then_some(ResolvedSort {
            column,
            descending: sort.descending,
        })
    }

    /// One member's matching rows, in snapshot order. The filter is [`crate::view::compute`], so
    /// it treats masked columns as a single-cluster list does.
    fn filter_member(&self, member: usize, query: &CombinedQuery, now: i64) -> Vec<u32> {
        let view = &self.members[member];
        let needle = query.text.trim().to_ascii_lowercase();
        let text = if view.display_name.to_ascii_lowercase().contains(&needle) {
            String::new()
        } else {
            query.text.clone()
        };
        crate::view::compute(
            &view.snapshot,
            &ViewQuery {
                namespaces: query.namespaces.clone(),
                text,
                sort: None,
            },
            now,
        )
    }

    fn order(&self, rows: &mut Vec<RowRef>, sort: Option<ResolvedSort>, now: i64) {
        // The rows are one sorted run per member, which a stable sort merges in O(n log members).
        rows.sort_by(|&a, &b| {
            let (a, b) = (self.row_at(a), self.row_at(b));
            (a.namespace.as_deref(), &*a.name).cmp(&(b.namespace.as_deref(), &*b.name))
        });
        let Some(sort) = sort else { return };
        let mut keyed: Vec<(SortKey<'_>, RowRef)> = rows
            .iter()
            .map(|&at| (self.sort_key(at, sort.column, now), at))
            .collect();
        keyed.sort_by(|(a, _), (b, _)| a.compare(b, sort.descending));
        rows.clear();
        rows.extend(keyed.into_iter().map(|(_, at)| at));
    }

    fn row_at(&self, at: RowRef) -> &Row {
        &self.members[at.member()].snapshot.rows[at.row as usize]
    }

    /// The ordering of [`crate::view::compare_cells`], except that integers stay exact.
    fn sort_key(&self, at: RowRef, column: usize, now: i64) -> SortKey<'_> {
        if column == CLUSTER_COLUMN {
            return SortKey::Int(i64::from(at.member));
        }
        let Slot::Shown(index) = self.members[at.member()].slots[column] else {
            return SortKey::Empty;
        };
        let row = self.row_at(at);
        match row.cells.get(index as usize) {
            None | Some(Cell::Null) => SortKey::Empty,
            Some(Cell::Name) => SortKey::Text(&row.name),
            Some(Cell::Namespace) => row
                .namespace
                .as_deref()
                .map_or(SortKey::Empty, SortKey::Text),
            Some(Cell::Age) => row.created.map_or(SortKey::Empty, |created| {
                SortKey::Int(now.saturating_sub(created))
            }),
            Some(Cell::Time(t)) => SortKey::Int(now.saturating_sub(*t)),
            Some(Cell::Int(i)) => SortKey::Int(*i),
            Some(Cell::Float(f)) => SortKey::Float(*f),
            Some(Cell::Bool(b)) => SortKey::Int(i64::from(*b)),
            Some(Cell::Text(s)) if s.is_empty() || &**s == "<none>" => SortKey::Empty,
            Some(Cell::Text(s)) => SortKey::Text(s),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolvedSort {
    column: usize,
    descending: bool,
}

enum SortKey<'a> {
    Empty,
    Int(i64),
    Float(f64),
    Text(&'a str),
}

impl SortKey<'_> {
    fn compare(&self, other: &Self, descending: bool) -> Ordering {
        let ord = match (self, other) {
            (SortKey::Empty, SortKey::Empty) => return Ordering::Equal,
            (SortKey::Empty, _) => return Ordering::Greater,
            (_, SortKey::Empty) => return Ordering::Less,
            (SortKey::Int(x), SortKey::Int(y)) => x.cmp(y),
            (SortKey::Float(x), SortKey::Float(y)) => x.total_cmp(y),
            (SortKey::Int(x), SortKey::Float(y)) => compare_int_float(*x, *y),
            (SortKey::Float(x), SortKey::Int(y)) => compare_int_float(*y, *x).reverse(),
            (SortKey::Text(x), SortKey::Text(y)) => natural_cmp(x, y),
            (SortKey::Text(_), _) => Ordering::Greater,
            (_, SortKey::Text(_)) => Ordering::Less,
        };
        if descending { ord.reverse() } else { ord }
    }
}

/// Compares exactly, with no rounding of `int`. Agrees with [`f64::total_cmp`] so that sorting
/// sees one total order: a negative NaN is below every integer, a positive NaN above, and `-0.0`
/// just below the integer 0.
fn compare_int_float(int: i64, float: f64) -> Ordering {
    /// 2^63, the first float above every `i64`.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if float.is_nan() {
        return if float.is_sign_negative() {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    if float >= LIMIT {
        return Ordering::Less;
    }
    if float < -LIMIT {
        return Ordering::Greater;
    }
    let whole = float.trunc();
    int.cmp(&(whole as i64)).then_with(|| {
        let fraction = float - whole;
        if fraction > 0.0 {
            Ordering::Less
        } else if fraction < 0.0 || (float == 0.0 && float.is_sign_negative()) {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    })
}

/// Each column's key, numbering repeats of one key within the member.
fn column_keys(columns: &[Column]) -> Vec<ColumnKey> {
    let mut seen: HashMap<String, u32> = HashMap::with_capacity(columns.len());
    columns
        .iter()
        .map(|column| {
            let key = column.key();
            let count = seen.entry(key.clone()).or_default();
            let occurrence = *count;
            *count += 1;
            ColumnKey { key, occurrence }
        })
        .collect()
}

/// The Cluster column, then every member's columns. A column new to the union lands just before
/// the next column its member shares with the union, so `Name, Status, Age` and
/// `Name, Node, Age` give `Name, Status, Node, Age`. Linear in the columns, per member.
fn union_columns(members: &[CombinedMember]) -> Vec<CombinedColumn> {
    let mut merged: Vec<CombinedColumn> = Vec::new();
    for member in members {
        let columns = &member.snapshot.columns;
        let keys = column_keys(columns);
        let positions: HashMap<ColumnKey, usize> = merged
            .iter()
            .enumerate()
            .map(|(i, c)| (c.key.clone(), i))
            .collect();
        for (key, column) in keys.iter().zip(columns.iter()) {
            if let Some(&at) = positions.get(key) {
                widen(&mut merged[at], column);
            }
        }
        let mut old: Vec<Option<CombinedColumn>> =
            std::mem::take(&mut merged).into_iter().map(Some).collect();
        let mut cursor = 0;
        let mut new_here: Vec<CombinedColumn> = Vec::new();
        for (key, column) in keys.into_iter().zip(columns.iter()) {
            match positions.get(&key) {
                Some(&at) if at >= cursor => {
                    merged.extend(old[cursor..at].iter_mut().filter_map(Option::take));
                    merged.append(&mut new_here);
                    merged.extend(old[at].take());
                    cursor = at + 1;
                }
                Some(_) => {}
                None => new_here.push(CombinedColumn {
                    key,
                    name: column.name.clone(),
                    ty: column.ty,
                    format: column.format.clone(),
                    description: column.description.clone(),
                    priority: column.priority,
                    role: Some(column.role),
                }),
            }
        }
        merged.extend(old[cursor..].iter_mut().filter_map(Option::take));
        merged.append(&mut new_here);
    }
    let mut all = Vec::with_capacity(merged.len() + 1);
    all.push(CombinedColumn {
        key: ColumnKey {
            key: CLUSTER_COLUMN_KEY.to_owned(),
            occurrence: 0,
        },
        name: "Cluster".into(),
        ty: ColumnType::String,
        format: "".into(),
        description: "".into(),
        priority: 0,
        role: None,
    });
    all.append(&mut merged);
    all
}

fn widen(into: &mut CombinedColumn, column: &Column) {
    into.priority = into.priority.min(column.priority);
    if into.ty != column.ty {
        into.ty = ColumnType::Other;
        into.format = "".into();
    }
}

#[cfg(test)]
#[path = "combined_tests.rs"]
mod tests;
