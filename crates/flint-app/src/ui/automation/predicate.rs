//! `wait_for` predicates: one test on one value of the `state` tree, found by JSON Pointer
//! (RFC 6901), such as `/list/shown` or `/detail/name`.

use serde_json::Value;

/// One test on the value at `pointer`.
#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    pointer: String,
    test: Test,
}

#[derive(Debug, Clone, PartialEq)]
enum Test {
    /// The value equals this (numbers compare by value: 2 equals 2.0).
    Equals(Value),
    NotEquals(Value),
    /// A number, or an array's length, is at least this.
    AtLeast(f64),
    AtMost(f64),
    /// The value is there and not null (`true`), or missing or null (`false`).
    Exists(bool),
    /// A string contains this text, or an array has an element matching this (see `matches`).
    Contains(Value),
    /// An array has an element matching this object: every field given, recursively.
    Any(Value),
}

/// The tests `wait_for` takes, as its arguments name them.
pub const TESTS: [&str; 7] = [
    "equals",
    "not_equals",
    "at_least",
    "at_most",
    "exists",
    "contains",
    "any",
];

impl Predicate {
    /// A predicate that the value at `pointer` exists; refine it with a test.
    #[cfg(test)]
    pub fn new(pointer: &str) -> Self {
        Self {
            pointer: pointer.to_owned(),
            test: Test::Exists(true),
        }
    }

    #[cfg(test)]
    pub fn at_least(mut self, n: f64) -> Self {
        self.test = Test::AtLeast(n);
        self
    }

    /// Builds a predicate from `wait_for` arguments: a `pointer` and exactly one test.
    pub fn from_args(pointer: String, test: &str, value: Value) -> Result<Self, String> {
        if !pointer.is_empty() && !pointer.starts_with('/') {
            return Err(format!(
                "pointer \"{pointer}\" must be empty or start with /"
            ));
        }
        let number = |value: &Value| {
            value
                .as_f64()
                .ok_or_else(|| format!("{test} takes a number"))
        };
        let test = match test {
            "equals" => Test::Equals(value),
            "not_equals" => Test::NotEquals(value),
            "at_least" => Test::AtLeast(number(&value)?),
            "at_most" => Test::AtMost(number(&value)?),
            "exists" => Test::Exists(value.as_bool().ok_or("exists takes true or false")?),
            "contains" => Test::Contains(value),
            "any" if value.is_object() => Test::Any(value),
            "any" => return Err("any takes an object: the fields an element must have".to_owned()),
            other => return Err(format!("unknown test \"{other}\"")),
        };
        Ok(Self { pointer, test })
    }

    /// Whether the predicate holds on `state`.
    pub fn holds(&self, state: &Value) -> bool {
        let value = state.pointer(&self.pointer).filter(|v| !v.is_null());
        match (&self.test, value) {
            (Test::Exists(wanted), value) => value.is_some() == *wanted,
            (Test::NotEquals(expected), None) => !expected.is_null(),
            (_, None) => matches!(&self.test, Test::Equals(Value::Null)),
            (Test::Equals(expected), Some(value)) => same(value, expected),
            (Test::NotEquals(expected), Some(value)) => !same(value, expected),
            (Test::AtLeast(n), Some(value)) => measure(value).is_some_and(|v| v >= *n),
            (Test::AtMost(n), Some(value)) => measure(value).is_some_and(|v| v <= *n),
            (Test::Contains(needle), Some(Value::String(text))) => {
                needle.as_str().is_some_and(|n| text.contains(n))
            }
            (Test::Contains(needle) | Test::Any(needle), Some(Value::Array(items))) => {
                items.iter().any(|item| matches(item, needle))
            }
            (Test::Contains(_) | Test::Any(_), Some(_)) => false,
        }
    }

    /// What the predicate saw, for a timeout message: the value (shortened to about
    /// [`SHOWN`] characters), or, for a pointer that leads nowhere, the deepest part of it that
    /// exists and its keys.
    pub fn observed(&self, state: &Value) -> String {
        if let Some(value) = state.pointer(&self.pointer) {
            return format!("it is {}", shorten(&value.to_string()));
        }
        let mut found = "";
        for (i, _) in self.pointer.match_indices('/').skip(1) {
            if state.pointer(&self.pointer[..i]).is_some() {
                found = &self.pointer[..i];
            }
        }
        let keys = match state.pointer(found) {
            Some(Value::Object(map)) => {
                let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
                keys.sort_unstable();
                format!("has keys [{}]", keys.join(", "))
            }
            Some(Value::Array(items)) => format!("has {} elements", items.len()),
            _ => "is not an object".to_owned(),
        };
        let at = if found.is_empty() { "the state" } else { found };
        format!("no value at {}; {at} {keys}", self.pointer)
    }

    pub fn pointer(&self) -> &str {
        &self.pointer
    }
}

/// How much of a value a timeout message shows.
pub const SHOWN: usize = 300;

fn shorten(text: &str) -> String {
    match text.char_indices().nth(SHOWN) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// Whether `item` matches `needle`: an object matches when every field of `needle` matches the
/// same field of `item` (so `{"name": "web"}` matches a row); anything else by value.
fn matches(item: &Value, needle: &Value) -> bool {
    match (item, needle) {
        (Value::Object(item), Value::Object(needle)) => needle
            .iter()
            .all(|(key, want)| item.get(key).is_some_and(|got| matches(got, want))),
        _ => same(item, needle),
    }
}

/// Equality, with numbers compared by value.
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// A number, or an array's length.
fn measure(value: &Value) -> Option<f64> {
    match value {
        Value::Array(items) => Some(items.len() as f64),
        other => other.as_f64(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Predicate;

    fn check(pointer: &str, test: &str, value: serde_json::Value) -> bool {
        let state = json!({
            "list": {"shown": 3, "rows": [
                {"name": "a", "cells_by_column": {"Status": "Running", "Ready": "1/1"}},
                {"name": "b"}
            ], "filter": "web-1"},
            "detail": null,
            "active_cluster": "kind-flint-dev",
        });
        Predicate::from_args(pointer.into(), test, value)
            .unwrap()
            .holds(&state)
    }

    #[test]
    fn tests_values_by_pointer() {
        assert!(check("/list/shown", "equals", json!(3)));
        assert!(check("/list/shown", "equals", json!(3.0)));
        assert!(!check("/list/shown", "equals", json!(2)));
        assert!(check("/list/shown", "not_equals", json!(2)));
        assert!(check("/list/shown", "at_least", json!(3)));
        assert!(!check("/list/shown", "at_most", json!(2)));
        assert!(check("/list/rows", "at_least", json!(2)));
        assert!(check("/list/rows/1/name", "equals", json!("b")));
        assert!(check("/list/filter", "contains", json!("web")));
        assert!(check("/list/rows", "contains", json!({"name": "a"})));
        assert!(check("/list/rows", "contains", json!({"name": "b"})));
        assert!(!check("/list/rows", "contains", json!({"name": "c"})));
        assert!(check(
            "/list/rows",
            "any",
            json!({"name": "a", "cells_by_column": {"Status": "Running"}})
        ));
        assert!(!check(
            "/list/rows",
            "any",
            json!({"name": "a", "cells_by_column": {"Status": "Pending"}})
        ));
        assert!(check("/active_cluster", "exists", json!(true)));
        assert!(check("/detail", "exists", json!(false)));
        assert!(check("/nothing", "exists", json!(false)));
        assert!(check("/detail", "equals", json!(null)));
        assert!(check("/detail", "not_equals", json!("x")));
        assert!(!check("/nothing", "at_least", json!(0)));
    }

    #[test]
    fn rejects_malformed_predicates() {
        assert!(Predicate::from_args("list".into(), "exists", json!(true)).is_err());
        assert!(Predicate::from_args("/x".into(), "at_least", json!("2")).is_err());
        assert!(Predicate::from_args("/x".into(), "exists", json!(1)).is_err());
        assert!(Predicate::from_args("/x".into(), "near", json!(1)).is_err());
        assert!(Predicate::from_args("/x".into(), "any", json!("a")).is_err());
    }

    #[test]
    fn says_what_it_saw() {
        let state = json!({"list": {"shown": 3, "rows": []}, "name": "x".repeat(400)});
        let seen = |pointer: &str| {
            Predicate::from_args(pointer.into(), "exists", json!(true))
                .unwrap()
                .observed(&state)
        };
        assert_eq!(seen("/list/shown"), "it is 3");
        assert_eq!(
            seen("/list/row"),
            "no value at /list/row; /list has keys [rows, shown]"
        );
        assert_eq!(
            seen("/lsit/shown"),
            "no value at /lsit/shown; the state has keys [list, name]"
        );
        assert!(seen("/name").chars().count() < 320);
    }
}
