//! Field and label selectors, evaluated on JSON objects the way the API server does for the
//! cases clients use: `a.b=c`, `a.b==c`, `a.b!=c` for fields; equality, existence and `in` /
//! `notin` sets for labels.

use serde_json::Value;

/// A parsed `fieldSelector`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSelector(Vec<(String, bool, String)>);

impl FieldSelector {
    pub fn parse(text: &str) -> FieldSelector {
        let terms = text
            .split(',')
            .filter(|t| !t.is_empty())
            .filter_map(|term| {
                if let Some((path, value)) = term.split_once("!=") {
                    Some((path.to_owned(), false, value.to_owned()))
                } else {
                    let (path, value) = term.split_once("==").or_else(|| term.split_once('='))?;
                    Some((path.to_owned(), true, value.to_owned()))
                }
            })
            .collect();
        FieldSelector(terms)
    }

    /// Whether `object` matches every term. A missing field compares as an empty string.
    pub fn matches(&self, object: &Value) -> bool {
        self.0.iter().all(|(path, equal, value)| {
            let found = path
                .split('.')
                .try_fold(object, |v, key| v.get(key))
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            (found == *value) == *equal
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LabelTerm {
    Equals(String, String),
    NotEquals(String, String),
    Exists(String),
    NotExists(String),
    In(String, Vec<String>),
    NotIn(String, Vec<String>),
}

/// A parsed `labelSelector`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelSelector(Vec<LabelTerm>);

impl LabelSelector {
    pub fn parse(text: &str) -> LabelSelector {
        LabelSelector(
            split_outside_parens(text)
                .iter()
                .filter_map(|t| term(t))
                .collect(),
        )
    }

    /// Whether the labels of `object` (`metadata.labels`) match every term.
    pub fn matches(&self, object: &Value) -> bool {
        let label = |key: &str| {
            object
                .pointer("/metadata/labels")
                .and_then(|l| l.get(key))
                .and_then(Value::as_str)
        };
        self.0.iter().all(|t| match t {
            LabelTerm::Equals(k, v) => label(k) == Some(v),
            LabelTerm::NotEquals(k, v) => label(k) != Some(v),
            LabelTerm::Exists(k) => label(k).is_some(),
            LabelTerm::NotExists(k) => label(k).is_none(),
            LabelTerm::In(k, vs) => label(k).is_some_and(|l| vs.iter().any(|v| v == l)),
            LabelTerm::NotIn(k, vs) => label(k).is_none_or(|l| vs.iter().all(|v| v != l)),
        })
    }
}

fn split_outside_parens(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

fn term(text: &str) -> Option<LabelTerm> {
    let set = |rest: &str| -> Vec<String> {
        rest.trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(',')
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .collect()
    };
    if let Some((key, rest)) = text.split_once(" notin ") {
        return Some(LabelTerm::NotIn(key.trim().to_owned(), set(rest)));
    }
    if let Some((key, rest)) = text.split_once(" in ") {
        return Some(LabelTerm::In(key.trim().to_owned(), set(rest)));
    }
    if let Some((k, v)) = text.split_once("!=") {
        return Some(LabelTerm::NotEquals(
            k.trim().to_owned(),
            v.trim().to_owned(),
        ));
    }
    if let Some((k, v)) = text.split_once("==").or_else(|| text.split_once('=')) {
        return Some(LabelTerm::Equals(k.trim().to_owned(), v.trim().to_owned()));
    }
    match text.strip_prefix('!') {
        Some(key) => Some(LabelTerm::NotExists(key.trim().to_owned())),
        None => Some(LabelTerm::Exists(text.trim().to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn field_selectors_compare_dotted_paths() {
        let event = json!({"involvedObject": {"uid": "u1", "kind": "Pod"},
                           "metadata": {"name": "e", "namespace": "demo"}, "count": 3});
        assert!(FieldSelector::parse("involvedObject.uid=u1").matches(&event));
        assert!(
            FieldSelector::parse("involvedObject.uid==u1,metadata.namespace=demo").matches(&event)
        );
        assert!(!FieldSelector::parse("involvedObject.uid=u2").matches(&event));
        assert!(FieldSelector::parse("involvedObject.kind!=Node").matches(&event));
        assert!(FieldSelector::parse("count=3").matches(&event));
        assert!(FieldSelector::parse("spec.nodeName=").matches(&event));
        assert!(FieldSelector::parse("").matches(&event));
    }

    #[test]
    fn label_selectors_support_equality_existence_and_sets() {
        let pod = json!({"metadata": {"labels": {"app": "web", "tier": "front"}}});
        let matches = |s: &str| LabelSelector::parse(s).matches(&pod);
        assert!(matches("app=web"));
        assert!(matches("app==web,tier"));
        assert!(!matches("app!=web"));
        assert!(matches("!missing"));
        assert!(!matches("missing"));
        assert!(matches("app in (web, api),tier notin (back)"));
        assert!(!matches("app notin (web)"));
        assert!(!matches("missing in (x)"));
        assert!(matches("missing notin (x)"));
        assert!(matches(""));
    }
}
