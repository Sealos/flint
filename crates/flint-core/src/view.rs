//! Namespace filter, text filter and sort over a snapshot, for list views. Pure functions; run
//! them off the UI thread with [`crate::CoreHandle::compute_view`].

use std::cmp::Ordering;

use crate::store::KindSnapshot;
use crate::table::{Cell, Row};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewQuery {
    /// Only rows in these namespaces. Empty means all namespaces. Ignored for cluster-scoped
    /// kinds.
    pub namespaces: Vec<String>,
    /// Case-insensitive substring of the name, namespace, any cell text, or a `key=value` label.
    pub text: String,
    pub sort: Option<SortBy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortBy {
    /// Index into [`KindSnapshot::columns`]. Columns can change between snapshots (an edited
    /// CRD); look the index up again by name when [`KindSnapshot::columns`] is a different
    /// `Arc` than before.
    pub column: usize,
    pub descending: bool,
}

/// Indexes into `snapshot.rows` of the rows that pass `query`, in display order. Rows that tie
/// on the sort column keep snapshot order (namespace, then name). `now` is Unix seconds.
pub fn compute(snapshot: &KindSnapshot, query: &ViewQuery, now: i64) -> Vec<u32> {
    let mut picked: Vec<u32> = if query.namespaces.is_empty() || !snapshot.namespaced {
        (0..snapshot.rows.len() as u32).collect()
    } else {
        let mut ranges: Vec<_> = query
            .namespaces
            .iter()
            .map(|n| snapshot.namespace_range(n))
            .collect();
        ranges.sort_by_key(|r| r.start);
        ranges.dedup();
        ranges.into_iter().flatten().map(|i| i as u32).collect()
    };
    let needle = query.text.trim().to_ascii_lowercase();
    if !needle.is_empty() {
        picked.retain(|&i| matches_text(&snapshot.rows[i as usize], &needle, now));
    }
    if let Some(sort) = query.sort.filter(|s| s.column < snapshot.columns.len()) {
        picked.sort_by(|&a, &b| {
            let ord = compare_cells(
                &snapshot.rows[a as usize],
                &snapshot.rows[b as usize],
                sort.column,
                now,
            );
            if sort.descending { ord.reverse() } else { ord }
        });
    }
    picked
}

fn matches_text(row: &Row, needle: &str, now: i64) -> bool {
    if contains_ignore_case(&row.name, needle)
        || row
            .namespace
            .as_deref()
            .is_some_and(|n| contains_ignore_case(n, needle))
    {
        return true;
    }
    for (i, cell) in row.cells.iter().enumerate() {
        let hit = match cell {
            Cell::Text(s) => contains_ignore_case(s, needle),
            Cell::Int(_) | Cell::Float(_) | Cell::Bool(_) => {
                contains_ignore_case(&row.cell_text(i, now), needle)
            }
            _ => false,
        };
        if hit {
            return true;
        }
    }
    row.labels
        .iter()
        .any(|(k, v)| contains_ignore_case(&format!("{k}={v}"), needle))
}

/// Whether `haystack` contains `needle` (already lowercase), ignoring ASCII case.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len())
        .any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

/// Orders two rows by one column: numbers numerically, ages and times by age (youngest first),
/// text with embedded numbers compared as numbers ("pod-9" before "pod-10"). Empty cells last.
pub fn compare_cells<'r>(a: &'r Row, b: &'r Row, column: usize, now: i64) -> Ordering {
    let key = |r: &'r Row| -> SortKey<'r> {
        match r.cells.get(column) {
            None | Some(Cell::Null) => SortKey::Empty,
            Some(Cell::Name) => SortKey::Text(&r.name),
            Some(Cell::Namespace) => r.namespace.as_deref().map_or(SortKey::Empty, SortKey::Text),
            Some(Cell::Age) => r
                .created
                .map_or(SortKey::Empty, |c| SortKey::Num((now - c) as f64)),
            Some(Cell::Time(t)) => SortKey::Num((now - t) as f64),
            Some(Cell::Int(i)) => SortKey::Num(*i as f64),
            Some(Cell::Float(f)) => SortKey::Num(*f),
            Some(Cell::Bool(b)) => SortKey::Num(f64::from(u8::from(*b))),
            Some(Cell::Text(s)) if s.is_empty() || &**s == "<none>" => SortKey::Empty,
            Some(Cell::Text(s)) => SortKey::Text(s),
        }
    };
    match (key(a), key(b)) {
        (SortKey::Empty, SortKey::Empty) => Ordering::Equal,
        (SortKey::Empty, _) => Ordering::Greater,
        (_, SortKey::Empty) => Ordering::Less,
        (SortKey::Num(x), SortKey::Num(y)) => x.total_cmp(&y),
        (SortKey::Text(x), SortKey::Text(y)) => natural_cmp(x, y),
        (SortKey::Num(_), SortKey::Text(_)) => Ordering::Less,
        (SortKey::Text(_), SortKey::Num(_)) => Ordering::Greater,
    }
}

enum SortKey<'a> {
    Empty,
    Num(f64),
    Text(&'a str),
}

/// Compares text with runs of digits compared by value, ignoring ASCII case.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let da = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let db = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (na, nb) = (trim_zeros(&a[..da]), trim_zeros(&b[..db]));
                let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
                if ord != Ordering::Equal {
                    return ord;
                }
                a = &a[da..];
                b = &b[db..];
            }
            (Some(x), Some(y)) => {
                let ord = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let start = digits
        .iter()
        .position(|&d| d != b'0')
        .unwrap_or(digits.len());
    &digits[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{Column, ColumnRole, ColumnType};
    use std::sync::Arc;

    fn row(ns: &str, name: &str, status: &str, restarts: i64, created: i64) -> Arc<Row> {
        Arc::new(Row {
            uid: format!("{ns}/{name}").into(),
            name: name.into(),
            namespace: Some(ns.into()),
            resource_version: "1".into(),
            generation: None,
            created: Some(created),
            labels: vec![(Arc::from("app"), Arc::from("web"))].into(),
            cells: vec![
                Cell::Name,
                Cell::Namespace,
                Cell::Text(status.into()),
                Cell::Int(restarts),
                Cell::Age,
            ]
            .into(),
        })
    }

    fn snapshot() -> KindSnapshot {
        let col = |n: &str, role| Column {
            name: n.into(),
            ty: ColumnType::String,
            format: "".into(),
            description: "".into(),
            priority: 0,
            role,
        };
        KindSnapshot {
            namespaced: true,
            columns: vec![
                col("Name", ColumnRole::Name),
                col("Namespace", ColumnRole::Namespace),
                col("Status", ColumnRole::Value),
                col("Restarts", ColumnRole::Value),
                col("Age", ColumnRole::Age),
            ]
            .into(),
            rows: vec![
                row("a", "pod-10", "Running", 2, 100),
                row("a", "pod-9", "Pending", 10, 300),
                row("b", "db-0", "Running", 0, 200),
                row("c", "x", "CrashLoopBackOff", 7, 50),
            ],
            ..Default::default()
        }
    }

    fn names(s: &KindSnapshot, idx: &[u32]) -> Vec<String> {
        idx.iter()
            .map(|&i| s.rows[i as usize].name.to_string())
            .collect()
    }

    // covers UP-21
    #[test]
    fn filters_by_namespace_and_text() {
        let s = snapshot();
        let q = ViewQuery {
            namespaces: vec!["c".into(), "a".into()],
            ..Default::default()
        };
        assert_eq!(names(&s, &compute(&s, &q, 1000)), ["pod-10", "pod-9", "x"]);
        let q = ViewQuery {
            text: "RUNNING".into(),
            ..Default::default()
        };
        assert_eq!(names(&s, &compute(&s, &q, 1000)), ["pod-10", "db-0"]);
        let q = ViewQuery {
            text: "app=web".into(),
            namespaces: vec!["b".into()],
            ..Default::default()
        };
        assert_eq!(names(&s, &compute(&s, &q, 1000)), ["db-0"]);
        let q = ViewQuery {
            text: "10".into(),
            ..Default::default()
        };
        assert_eq!(
            names(&s, &compute(&s, &q, 1000)),
            ["pod-10", "pod-9"],
            "pod-9 has 10 restarts"
        );
    }

    // covers UP-21
    #[test]
    fn sorts_by_typed_columns() {
        let s = snapshot();
        let sorted = |column, descending| {
            let q = ViewQuery {
                sort: Some(SortBy { column, descending }),
                ..Default::default()
            };
            names(&s, &compute(&s, &q, 1000))
        };
        assert_eq!(
            sorted(0, false),
            ["db-0", "pod-9", "pod-10", "x"],
            "natural order"
        );
        assert_eq!(
            sorted(3, true),
            ["pod-9", "x", "pod-10", "db-0"],
            "restarts numeric"
        );
        assert_eq!(
            sorted(4, false),
            ["pod-9", "db-0", "pod-10", "x"],
            "youngest first"
        );
        // Ties keep namespace/name order.
        assert_eq!(sorted(2, false), ["x", "pod-9", "pod-10", "db-0"]);
    }

    #[test]
    fn natural_ordering() {
        assert_eq!(natural_cmp("a2", "a10"), Ordering::Less);
        assert_eq!(natural_cmp("a010", "a10"), Ordering::Equal);
        assert_eq!(natural_cmp("B", "a"), Ordering::Greater);
        assert_eq!(natural_cmp("1/1", "0/1"), Ordering::Greater);
    }
}
