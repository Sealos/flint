//! Which objects a telemetry pass judged, in eight bytes each, so a list can tell a healthy row
//! from one the feed has not seen yet.

use std::borrow::Cow;
use std::hash::{DefaultHasher, Hash, Hasher};

use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Resource;

use super::WatchedKind;

/// The identities of the objects judged, per kind, sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckedObjects {
    by_kind: [Box<[u64]>; WatchedKind::ALL.len()],
}

impl CheckedObjects {
    /// Records `items` as what was judged of `kind`. `None` records nothing.
    pub(super) fn record<K: Resource>(&mut self, kind: WatchedKind, items: Option<&[K]>) {
        let keys = items
            .into_iter()
            .flatten()
            .map(|item| identity(item.meta()));
        self.record_uids(kind, keys);
    }

    /// Records objects by the identity a list row carries as its uid.
    pub fn record_uids<S: AsRef<str>>(
        &mut self,
        kind: WatchedKind,
        uids: impl IntoIterator<Item = S>,
    ) {
        let mut keys: Vec<u64> = uids.into_iter().map(|uid| key(uid.as_ref())).collect();
        keys.sort_unstable();
        keys.dedup();
        self.by_kind[kind as usize] = keys.into();
    }

    /// Whether the pass judged the object a row of `kind` with this uid shows.
    pub fn contains(&self, kind: WatchedKind, uid: &str) -> bool {
        self.by_kind[kind as usize].binary_search(&key(uid)).is_ok()
    }
}

/// The identity a list row has for this object: its UID, else its namespaced name.
pub(super) fn identity(meta: &ObjectMeta) -> Cow<'_, str> {
    if let Some(uid) = meta.uid.as_deref().filter(|uid| !uid.is_empty()) {
        return Cow::Borrowed(uid);
    }
    let name = meta.name.as_deref().unwrap_or_default();
    match meta.namespace.as_deref().filter(|ns| !ns.is_empty()) {
        Some(namespace) => Cow::Owned(format!("{namespace}/{name}")),
        None => Cow::Borrowed(name),
    }
}

fn key(uid: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    uid.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::Pod;

    use super::*;

    fn pod(namespace: &str, name: &str, uid: Option<&str>) -> Pod {
        Pod {
            metadata: ObjectMeta {
                namespace: Some(namespace.to_owned()),
                name: Some(name.to_owned()),
                uid: uid.map(str::to_owned),
                ..ObjectMeta::default()
            },
            ..Pod::default()
        }
    }

    #[test]
    fn an_object_is_known_by_the_identity_its_row_carries() {
        let mut checked = CheckedObjects::default();
        let pods = [pod("ns", "a", Some("uid-a")), pod("ns", "b", None)];
        checked.record(WatchedKind::Pods, Some(&pods));
        assert!(checked.contains(WatchedKind::Pods, "uid-a"));
        assert!(checked.contains(WatchedKind::Pods, "ns/b"));
        assert!(!checked.contains(WatchedKind::Pods, "uid-recreated"));
        assert!(!checked.contains(WatchedKind::Nodes, "uid-a"));
    }
}
