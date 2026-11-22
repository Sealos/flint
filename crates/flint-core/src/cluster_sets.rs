//! Named cluster sets (ADR-0014): the typed `cluster_sets` setting in `settings.toml`.
//!
//! A set has a name and 2 to 5 members, each pinned to a context and its API server as a
//! [`ClusterPin`]. A context that now points elsewhere is reported as changed, never treated as
//! the same cluster. An entry this version cannot use is left out when loading and stays in the
//! file. Every change goes through the settings save path and its cross-process lock.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::display::{MAX_CUSTOM_NAME_CHARS, clean_custom_name, cluster_name_key};
use crate::settings::{
    ClusterPin, Settings, SettingsError, edit_settings_if_changed, load_settings,
};

/// The key of the setting in `settings.toml`.
pub(crate) const SECTION_KEY: &str = "cluster_sets";
/// The fewest clusters a set combines.
pub const MIN_CLUSTER_SET_MEMBERS: usize = 2;
/// The most clusters a set combines.
pub const MAX_CLUSTER_SET_MEMBERS: usize = 5;

/// A saved selection of clusters. Build one with [`ClusterSet::new`], which enforces the limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSet {
    pub name: String,
    pub members: Vec<ClusterPin>,
}

/// Where a member stands against the current kubeconfig.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberStatus {
    /// Its context still points at the pinned server.
    Current,
    /// Its context now points at another server, so it is not the cluster that was chosen.
    Changed,
    /// Its context is gone, or has no server.
    Missing,
}

impl ClusterSet {
    /// A set of `members` under `name` (trimmed), or why it cannot be one. The reason never
    /// quotes a server, which may hold credentials.
    pub fn new(name: &str, members: Vec<ClusterPin>) -> Result<Self, String> {
        let name = clean_custom_name(name).ok_or_else(|| {
            format!(
                "a cluster set name has 1 to {MAX_CUSTOM_NAME_CHARS} letters, digits or any of {}",
                crate::display::CUSTOM_NAME_PUNCTUATION
            )
        })?;
        if !(MIN_CLUSTER_SET_MEMBERS..=MAX_CLUSTER_SET_MEMBERS).contains(&members.len()) {
            return Err(format!(
                "a cluster set has {MIN_CLUSTER_SET_MEMBERS} to {MAX_CLUSTER_SET_MEMBERS} clusters"
            ));
        }
        if members
            .iter()
            .any(|m| m.context.trim().is_empty() || m.server.trim().is_empty())
        {
            return Err("each cluster in a set needs a context and a server".to_owned());
        }
        // A context points at one server, so a second pin for it could never be current.
        let contexts: BTreeSet<&str> = members.iter().map(|m| m.context.as_str()).collect();
        if contexts.len() != members.len() {
            return Err("a cluster set lists each context once".to_owned());
        }
        Ok(Self { name, members })
    }

    /// Each member with its status, in order. `current_server` gives the server a context points
    /// at now, or `None` when the context is gone or has no server. A blank server is no server.
    pub fn member_statuses<'s>(
        &self,
        current_server: impl Fn(&str) -> Option<&'s str>,
    ) -> Vec<(&ClusterPin, MemberStatus)> {
        self.members
            .iter()
            .map(|pin| {
                let server = current_server(&pin.context).filter(|s| !s.trim().is_empty());
                let status = match server {
                    Some(server) if pin.matches(&pin.context, server) => MemberStatus::Current,
                    Some(_) => MemberStatus::Changed,
                    None => MemberStatus::Missing,
                };
                (pin, status)
            })
            .collect()
    }

    /// The form in which two set names count as the same name.
    fn key(&self) -> String {
        cluster_name_key(&self.name)
    }
}

/// The saved sets in `settings.toml`, without the entries this version cannot use.
pub fn load_cluster_sets(dir: &Path) -> Result<Vec<ClusterSet>, SettingsError> {
    load_settings(dir).map(|settings| settings.cluster_sets)
}

/// Saves a new set. A name another set already goes by is an error. Returns the settings as
/// written.
pub fn create_cluster_set(
    dir: &Path,
    name: &str,
    members: &[ClusterPin],
) -> Result<Settings, SettingsError> {
    edit_entries(dir, |entries, path| {
        let set = ClusterSet::new(name, members.to_vec()).map_err(|r| invalid(path, r))?;
        if position(entries, &set.key()).is_some() {
            return Err(name_taken(path, &set.name));
        }
        entries.push(toml::Value::try_from(&set)?);
        Ok(true)
    })
}

/// Renames the set called `name`, dropping any hidden copy the file holds under that name. It may
/// change the case of its own name, but not take another set's. Entries this version cannot use
/// stay. Returns the settings as written.
pub fn rename_cluster_set(
    dir: &Path,
    name: &str,
    new_name: &str,
) -> Result<Settings, SettingsError> {
    edit_entries(dir, |entries, path| {
        let cleaned = clean_custom_name(new_name).ok_or_else(|| {
            invalid(
                path,
                format!("a cluster set name has 1 to {MAX_CUSTOM_NAME_CHARS} allowed characters"),
            )
        })?;
        let old_key = clean_custom_name(name).map(|old| cluster_name_key(&old));
        let index = old_key
            .as_deref()
            .and_then(|key| position(entries, key))
            .ok_or_else(|| invalid(path, "there is no cluster set with that name".to_owned()))?;
        let new_key = cluster_name_key(&cleaned);
        if position(entries, &new_key).is_some_and(|other| other != index) {
            return Err(name_taken(path, &cleaned));
        }
        // Hidden copies under the old name go, or one would appear as a set once this one moves.
        // `index` is the first readable holder of the name, so removing later ones keeps it valid.
        let mut at = 0;
        entries.retain(|entry| {
            let keep = at == index || readable(entry).is_none_or(|s| Some(s.key()) != old_key);
            at += 1;
            keep
        });
        let table = entries[index].as_table_mut().expect("a readable entry");
        table.insert("name".into(), toml::Value::String(cleaned));
        Ok(true)
    })
}

/// Deletes the set called `name`, with any copy of it the file holds under the same name, so
/// that a copy does not take its place. Entries this version cannot use stay. Deleting a set
/// that is not there writes nothing. Returns the settings as written.
pub fn delete_cluster_set(dir: &Path, name: &str) -> Result<Settings, SettingsError> {
    let Some(key) = clean_custom_name(name).map(|name| cluster_name_key(&name)) else {
        return load_settings(dir);
    };
    let settings = load_settings(dir)?;
    if !settings.cluster_sets.iter().any(|set| set.key() == key) {
        return Ok(settings);
    }
    remove_entries(dir, &key)
}

/// Removes the sets under `key` while holding the save lock. If another save got there first,
/// nothing is removed and the file is not written.
fn remove_entries(dir: &Path, key: &str) -> Result<Settings, SettingsError> {
    edit_entries(dir, |entries, _| {
        let before = entries.len();
        entries.retain(|entry| readable(entry).is_none_or(|set| set.key() != key));
        Ok(entries.len() != before)
    })
}

/// Edits the raw entries under the save lock, so entries this version cannot read survive. The
/// file is written only when `edit` returns `true`. A `cluster_sets` that is not a list is an
/// error and leaves the file as it is.
fn edit_entries(
    dir: &Path,
    edit: impl FnOnce(&mut Vec<toml::Value>, &Path) -> Result<bool, SettingsError>,
) -> Result<Settings, SettingsError> {
    edit_settings_if_changed(dir, |table, path| {
        let mut entries = match table.remove(SECTION_KEY) {
            Some(toml::Value::Array(entries)) => entries,
            None => Vec::new(),
            Some(_) => {
                return Err(invalid(path, format!("{SECTION_KEY} is not a list")));
            }
        };
        let changed = edit(&mut entries, path)?;
        if !entries.is_empty() {
            table.insert(SECTION_KEY.into(), toml::Value::Array(entries));
        }
        Ok(changed)
    })
}

/// The index of the entry that holds `key`: the first readable one, as loading keeps.
fn position(entries: &[toml::Value], key: &str) -> Option<usize> {
    entries
        .iter()
        .position(|entry| readable(entry).is_some_and(|set| set.key() == key))
}

/// `entry` as a set, if it is a valid one.
fn readable(entry: &toml::Value) -> Option<ClusterSet> {
    let set: ClusterSet = entry.clone().try_into().ok()?;
    ClusterSet::new(&set.name, set.members).ok()
}

/// The sets `value` holds: valid entries, the first of any that share a name.
fn visible_sets(value: &toml::Value) -> Vec<ClusterSet> {
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    let mut names = BTreeSet::new();
    entries
        .iter()
        .filter_map(readable)
        .filter(|set| names.insert(set.key()))
        .collect()
}

/// How many entries of `value` loading leaves out, counting a value that is not a list as one.
pub(crate) fn ignored_entries(value: &toml::Value) -> usize {
    value
        .as_array()
        .map_or(1, |entries| entries.len() - visible_sets(value).len())
}

/// Reads `cluster_sets` so that a bad entry costs only that entry, never the settings file.
pub(crate) fn readable_cluster_sets<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ClusterSet>, D::Error> {
    Ok(visible_sets(&toml::Value::deserialize(deserializer)?))
}

fn invalid(path: &Path, reason: String) -> SettingsError {
    SettingsError::Invalid {
        path: path.to_owned(),
        reason,
    }
}

fn name_taken(path: &Path, name: &str) -> SettingsError {
    invalid(
        path,
        format!("another cluster set already goes by {name:?}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{AiScope, SETTINGS_FILE, set_ai_exposed, set_trusted};

    fn pins(count: usize) -> Vec<ClusterPin> {
        (0..count)
            .map(|i| ClusterPin::new(format!("ctx-{i}"), format!("https://{i}.example:6443")))
            .collect()
    }

    fn names(settings: &Settings) -> Vec<&str> {
        settings
            .cluster_sets
            .iter()
            .map(|set| set.name.as_str())
            .collect()
    }

    fn reason(error: SettingsError) -> String {
        match error {
            SettingsError::Invalid { reason, .. } => reason,
            other => panic!("expected an invalid setting, got {other}"),
        }
    }

    fn write(dir: &Path, text: &str) {
        std::fs::write(dir.join(SETTINGS_FILE), text).unwrap();
    }

    fn read(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(SETTINGS_FILE)).unwrap()
    }

    #[test]
    fn a_set_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let written = create_cluster_set(dir.path(), "  prod regions ", &pins(3)).unwrap();
        let loaded = load_cluster_sets(dir.path()).unwrap();
        assert_eq!(loaded, written.cluster_sets);
        assert_eq!(loaded[0].name, "prod regions");
        assert_eq!(loaded[0].members, pins(3));
        assert!(read(dir.path()).contains("[[cluster_sets.members]]"));
    }

    #[test]
    fn member_limits_are_two_to_five() {
        let dir = tempfile::tempdir().unwrap();
        for count in [0, 1, 6] {
            let error = create_cluster_set(dir.path(), "set", &pins(count)).unwrap_err();
            assert!(reason(error).contains("2 to 5"), "{count} members");
        }
        assert!(!dir.path().join(SETTINGS_FILE).exists());
        create_cluster_set(dir.path(), "two", &pins(2)).unwrap();
        let settings = create_cluster_set(dir.path(), "five", &pins(5)).unwrap();
        assert_eq!(names(&settings), ["two", "five"]);
    }

    #[test]
    fn a_set_refuses_bad_names_and_members() {
        assert!(ClusterSet::new("", pins(2)).is_err());
        assert!(ClusterSet::new("bad\nname", pins(2)).is_err());
        assert!(ClusterSet::new(&"x".repeat(MAX_CUSTOM_NAME_CHARS + 1), pins(2)).is_err());

        let same_context = vec![
            ClusterPin::new("a", "https://one"),
            ClusterPin::new("a", "https://two"),
        ];
        assert!(ClusterSet::new("set", same_context).is_err());
        let no_server = vec![ClusterPin::new("a", " "), ClusterPin::new("b", "https://b")];
        assert!(ClusterSet::new("set", no_server).is_err());
    }

    #[test]
    fn duplicate_names_are_refused_on_create_and_rename() {
        let dir = tempfile::tempdir().unwrap();
        create_cluster_set(dir.path(), "Prod EU", &pins(2)).unwrap();
        create_cluster_set(dir.path(), "staging", &pins(2)).unwrap();

        let error = create_cluster_set(dir.path(), "prod-eu", &pins(3)).unwrap_err();
        assert!(reason(error).contains("already"));
        let error = rename_cluster_set(dir.path(), "staging", "PROD eu").unwrap_err();
        assert!(reason(error).contains("already"));

        let settings = load_settings(dir.path()).unwrap();
        assert_eq!(names(&settings), ["Prod EU", "staging"]);
    }

    #[test]
    fn rename_keeps_members_and_may_recase_its_own_name() {
        let dir = tempfile::tempdir().unwrap();
        create_cluster_set(dir.path(), "prod", &pins(4)).unwrap();
        let settings = rename_cluster_set(dir.path(), "PROD", "Prod").unwrap();
        assert_eq!(names(&settings), ["Prod"]);
        let settings = rename_cluster_set(dir.path(), "prod", "live").unwrap();
        assert_eq!(names(&settings), ["live"]);
        assert_eq!(settings.cluster_sets[0].members, pins(4));

        let error = rename_cluster_set(dir.path(), "gone", "other").unwrap_err();
        assert!(reason(error).contains("no cluster set"));
    }

    #[test]
    fn delete_removes_the_set_and_its_copies_only() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"
[[cluster_sets]]
name = "prod"
members = [{ context = "a", server = "https://a" }, { context = "b", server = "https://b" }]

[[cluster_sets]]
name = "PROD"
members = [{ context = "c", server = "https://c" }, { context = "d", server = "https://d" }]

[[cluster_sets]]
name = "prod"
members = [{ context = "only-one", server = "https://e" }]

[[cluster_sets]]
name = "keep"
members = [{ context = "a", server = "https://a" }, { context = "c", server = "https://c" }]
"#,
        );
        let settings = delete_cluster_set(dir.path(), "Prod").unwrap();
        assert_eq!(names(&settings), ["keep"]);
        assert!(
            read(dir.path()).contains("only-one"),
            "unusable entry stays"
        );
    }

    #[test]
    fn rename_does_not_bring_a_hidden_copy_into_view() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"
[[cluster_sets]]
name = "prod"
members = [{ context = "a", server = "https://a" }, { context = "b", server = "https://b" }]

[[cluster_sets]]
name = "PROD"
members = [{ context = "c", server = "https://c" }, { context = "d", server = "https://d" }]

[[cluster_sets]]
name = "prod"
members = [{ context = "only-one", server = "https://e" }]
"#,
        );
        let settings = rename_cluster_set(dir.path(), "prod", "live").unwrap();
        assert_eq!(names(&settings), ["live"]);
        assert_eq!(settings.cluster_sets[0].members[0].context, "a");
        assert!(
            read(dir.path()).contains("only-one"),
            "unusable entry stays"
        );
    }

    #[test]
    fn deleting_a_set_that_is_not_there_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let text = "# a comment\nappearance = \"dark\"\n";
        write(dir.path(), text);
        delete_cluster_set(dir.path(), "gone").unwrap();
        delete_cluster_set(dir.path(), "bad\nname").unwrap();
        assert_eq!(read(dir.path()), text);
    }

    #[test]
    fn a_delete_that_loses_the_race_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let key = cluster_name_key("gone");

        let settings = remove_entries(dir.path(), &key).unwrap();
        assert!(settings.cluster_sets.is_empty());
        assert!(
            !dir.path().join(SETTINGS_FILE).exists(),
            "a removed file is not recreated"
        );

        let text = "# a comment\nprotect_local_clusters = true\n\n[[cluster_sets]]\nname = \"keep\"\nmembers = [{ context = \"a\", server = \"https://a\" }, { context = \"b\", server = \"https://b\" }]\n";
        write(dir.path(), text);
        let settings = remove_entries(dir.path(), &key).unwrap();
        assert_eq!(names(&settings), ["keep"]);
        assert!(settings.protect_local_clusters);
        assert_eq!(read(dir.path()), text);
    }

    #[test]
    fn a_blank_server_is_missing_not_changed() {
        let set = ClusterSet::new(
            "set",
            vec![
                ClusterPin::new("empty", "https://empty:6443"),
                ClusterPin::new("spaces", "https://spaces:6443"),
            ],
        )
        .unwrap();
        let statuses = set.member_statuses(|context| match context {
            "empty" => Some(""),
            _ => Some("  \t"),
        });
        assert!(
            statuses
                .iter()
                .all(|(_, status)| *status == MemberStatus::Missing)
        );
    }

    #[test]
    fn a_repointed_member_is_changed_not_current() {
        let set = ClusterSet::new(
            "set",
            vec![
                ClusterPin::new("same", "https://same:6443"),
                ClusterPin::new("slash", "https://slash:6443"),
                ClusterPin::new("moved", "https://old:6443"),
                ClusterPin::new("gone", "https://gone:6443"),
            ],
        )
        .unwrap();
        let statuses = set.member_statuses(|context| match context {
            "same" => Some("https://same:6443"),
            "slash" => Some("https://slash:6443/"),
            "moved" => Some("https://new:6443"),
            _ => None,
        });
        let statuses: Vec<_> = statuses.into_iter().map(|(_, status)| status).collect();
        assert_eq!(
            statuses,
            [
                MemberStatus::Current,
                MemberStatus::Current,
                MemberStatus::Changed,
                MemberStatus::Missing,
            ]
        );
    }

    #[test]
    fn an_invalid_entry_costs_only_that_entry() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"
appearance = "dark"
protect_local_clusters = true
cluster_sets = [
    { name = "good", members = [{ context = "a", server = "https://a" }, { context = "b", server = "https://b" }] },
    { name = "one member", members = [{ context = "a", server = "https://a" }] },
    { name = "no members" },
    { members = [{ context = "a", server = "https://a" }, { context = "b", server = "https://b" }] },
    "not a table",
]
"#,
        );
        let settings = load_settings(dir.path()).unwrap();
        assert_eq!(names(&settings), ["good"]);
        assert!(settings.protect_local_clusters);
        assert_eq!(settings.appearance, crate::Appearance::Dark);

        let settings = create_cluster_set(dir.path(), "new", &pins(2)).unwrap();
        assert_eq!(names(&settings), ["good", "new"]);
        let text = read(dir.path());
        assert!(text.contains("one member") && text.contains("not a table"));
    }

    #[test]
    fn a_value_that_is_not_a_list_loads_and_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let text = "cluster_sets = 3\nprotect_local_clusters = true\n";
        write(dir.path(), text);
        let settings = load_settings(dir.path()).unwrap();
        assert!(settings.cluster_sets.is_empty());
        assert!(settings.protect_local_clusters);

        let error = create_cluster_set(dir.path(), "set", &pins(2)).unwrap_err();
        assert!(reason(error).contains("not a list"));
        assert_eq!(read(dir.path()), text);
    }

    #[test]
    fn saving_keeps_protection_and_exposure_settings() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "protect_local_clusters = true\n");
        let scope = AiScope::new(vec!["team-a".into()], vec!["pods".into()]);
        set_ai_exposed(dir.path(), "exposed", Some("https://exposed"), &scope).unwrap();
        set_trusted(dir.path(), "trusted", Some("https://trusted")).unwrap();

        create_cluster_set(dir.path(), "first", &pins(2)).unwrap();
        rename_cluster_set(dir.path(), "first", "second").unwrap();
        create_cluster_set(dir.path(), "third", &pins(3)).unwrap();
        let settings = delete_cluster_set(dir.path(), "second").unwrap();

        assert_eq!(names(&settings), ["third"]);
        assert!(settings.protect_local_clusters);
        assert!(
            settings
                .trust("trusted", Some("https://trusted"))
                .is_trusted()
        );
        assert_eq!(
            settings.ai_scope("exposed", Some("https://exposed")),
            Some(scope)
        );
    }

    #[test]
    fn concurrent_saves_all_land() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|threads| {
            for i in 0..4 {
                let dir = dir.path();
                threads.spawn(move || {
                    create_cluster_set(dir, &format!("set {i}"), &pins(2)).unwrap();
                });
            }
        });
        assert_eq!(load_cluster_sets(dir.path()).unwrap().len(), 4);
    }
}
