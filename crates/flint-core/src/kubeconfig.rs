//! Loading and merging kubeconfig files with kubectl semantics.
//!
//! Sources: the `KUBECONFIG` list if set, else `~/.kube/config`, then extra paths from settings.
//! Files are merged in order and the first file to define a name wins (`Kubeconfig::merge`).
//! Every context becomes a Flint cluster, and Flint records which file each one came from.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use kube::config::{Kubeconfig, NamedContext};

use crate::settings::{Settings, expand_tilde};

/// One kubeconfig context, as Flint shows it in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextInfo {
    /// The context name. This is the cluster's identity in Flint.
    pub name: String,
    /// The kubeconfig `cluster` entry the context points at.
    pub cluster: Option<String>,
    /// The kubeconfig `user` entry the context points at.
    pub user: Option<String>,
    pub namespace: Option<String>,
    /// The API server URL, if the referenced cluster entry exists.
    pub server: Option<String>,
    /// The file whose definition of this context won the merge.
    pub source: PathBuf,
}

impl ContextInfo {
    /// The host (and port, if any) of the API server URL, for compact display.
    pub fn server_host(&self) -> Option<&str> {
        let server = self.server.as_deref()?;
        let rest = server.split_once("://").map_or(server, |(_, r)| r);
        Some(rest.split('/').next().unwrap_or(rest))
    }
}

/// The result of loading every kubeconfig source.
#[derive(Debug, Clone, Default)]
pub struct LoadedKubeconfig {
    /// The merged config used to build clients.
    pub merged: Kubeconfig,
    /// Contexts in merge order.
    pub contexts: Vec<ContextInfo>,
    /// Every path Flint looked at, existing or not. These are the files to watch, and the list
    /// given to `helm` as `KUBECONFIG`.
    pub paths: Vec<PathBuf>,
    /// Files that exist but could not be read or parsed. They are skipped, like kubectl's
    /// behavior for a missing file, but reported so the UI can show them.
    pub errors: Vec<(PathBuf, String)>,
}

impl LoadedKubeconfig {
    pub fn context(&self, name: &str) -> Option<&ContextInfo> {
        self.contexts.iter().find(|c| c.name == name)
    }
}

/// The ordered list of kubeconfig files, following kubectl: [`base_paths`] then `extra` paths
/// from settings. Duplicates are dropped, keeping the first occurrence.
pub fn kubeconfig_paths(
    env_kubeconfig: Option<&OsStr>,
    home: Option<&Path>,
    extra: &[PathBuf],
) -> Vec<PathBuf> {
    append_paths(&base_paths(env_kubeconfig, home), extra)
}

/// The files kubectl would read. `env_kubeconfig` is the value of `KUBECONFIG`: if set and
/// non-empty, its colon-separated entries are used and `~/.kube/config` is not. Empty entries are
/// ignored. The value is split as raw bytes, so a path that is not UTF-8 is still used rather than
/// falling back to `~/.kube/config`.
pub fn base_paths(env_kubeconfig: Option<&OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
    match env_kubeconfig.filter(|s| !s.is_empty()) {
        Some(list) => std::env::split_paths(list)
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| expand_tilde(&p, home))
            .collect(),
        None => home
            .map(|home| home.join(".kube").join("config"))
            .into_iter()
            .collect(),
    }
}

/// `base` followed by `extra`, without duplicates (the first occurrence is kept).
pub fn append_paths(base: &[PathBuf], extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    base.iter()
        .chain(extra)
        .filter(|p| seen.insert((*p).clone()))
        .cloned()
        .collect()
}

/// `base` plus the settings' `kubeconfig_paths`: the files to load for these settings.
pub fn paths_for_settings(
    base: &[PathBuf],
    settings: &Settings,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    append_paths(base, &settings.expanded_kubeconfig_paths(home))
}

/// [`base_paths`] for this process: `KUBECONFIG` or `~/.kube/config`.
pub fn default_base_paths() -> Vec<PathBuf> {
    base_paths(
        std::env::var_os("KUBECONFIG").as_deref(),
        std::env::home_dir().as_deref(),
    )
}

/// A one-line summary of a kubeconfig load error: its first non-empty line, at most 200
/// characters. Parse errors span several lines and can quote the file, which may hold
/// credentials, so callers show this and keep the full text out of agent output.
pub fn error_summary(error: &str) -> String {
    crate::redact::first_line(error, 200)
}

/// What [`load`] depends on: each file's modification time and size, or `None` if it does not
/// exist. When the stamps of two loads are equal, so are the
/// results, which lets callers that load often (the MCP server) skip re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamps(Vec<(PathBuf, Option<(std::time::SystemTime, u64)>)>);

impl FileStamps {
    pub fn of(paths: &[PathBuf]) -> Self {
        FileStamps(
            paths
                .iter()
                .map(|p| {
                    let stamp = std::fs::metadata(p)
                        .ok()
                        .and_then(|m| Some((m.modified().ok()?, m.len())));
                    (p.clone(), stamp)
                })
                .collect(),
        )
    }
}

/// Reads and merges `paths` in order. Missing files are skipped silently; unreadable or invalid
/// files are skipped and listed in `errors`.
///
/// Names are resolved first-wins, across files and within one file, as kubectl documents for
/// merging. Each file's `kind` and `apiVersion` are dropped before merging: they are legacy
/// fields kubectl does not check, and a mismatch would otherwise reject a valid file.
pub fn load(paths: &[PathBuf]) -> LoadedKubeconfig {
    let mut merged = Kubeconfig::default();
    let mut contexts = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let mut config = match Kubeconfig::read_from(path) {
            Ok(config) => config,
            Err(e) => {
                errors.push((path.clone(), e.to_string()));
                continue;
            }
        };
        config.kind = None;
        config.api_version = None;
        dedupe_by_name(&mut config.clusters, |c| &c.name);
        dedupe_by_name(&mut config.auth_infos, |u| &u.name);
        dedupe_by_name(&mut config.contexts, |c| &c.name);
        let known: HashSet<String> = merged.contexts.iter().map(|c| c.name.clone()).collect();
        let new_contexts: Vec<NamedContext> = config
            .contexts
            .iter()
            .filter(|c| !known.contains(&c.name))
            .cloned()
            .collect();
        merged = match merged.merge(config) {
            Ok(m) => m,
            Err(e) => {
                // Not expected now that kind and apiVersion are cleared, but `merge` may grow new
                // checks. It consumed the accumulated config, so report the file and rebuild from
                // the remaining sources.
                errors.push((path.clone(), e.to_string()));
                return load_without(paths, path, errors);
            }
        };
        for named in new_contexts {
            contexts.push(ContextInfo {
                cluster: named.context.as_ref().map(|c| c.cluster.clone()),
                user: named.context.as_ref().and_then(|c| c.user.clone()),
                namespace: named.context.as_ref().and_then(|c| c.namespace.clone()),
                server: None,
                name: named.name,
                source: path.clone(),
            });
        }
    }
    for info in &mut contexts {
        info.server = info.cluster.as_ref().and_then(|cluster| {
            merged
                .clusters
                .iter()
                .find(|c| &c.name == cluster)
                .and_then(|c| c.cluster.as_ref())
                .and_then(|c| c.server.clone())
        });
    }
    LoadedKubeconfig {
        merged,
        contexts,
        paths: paths.to_vec(),
        errors,
    }
}

/// Keeps the first entry for each name.
fn dedupe_by_name<T>(items: &mut Vec<T>, name: impl Fn(&T) -> &String) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(name(item).clone()));
}

/// Reloads with one file left out, after it failed to merge.
fn load_without(
    paths: &[PathBuf],
    bad: &Path,
    mut errors: Vec<(PathBuf, String)>,
) -> LoadedKubeconfig {
    let remaining: Vec<PathBuf> = paths.iter().filter(|p| *p != bad).cloned().collect();
    let mut loaded = load(&remaining);
    errors.append(&mut loaded.errors);
    loaded.errors = errors;
    loaded.paths = paths.to_vec();
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn config(contexts: &[(&str, &str, &str)], clusters: &[(&str, &str)]) -> String {
        let mut s = String::from("apiVersion: v1\nkind: Config\nclusters:\n");
        for (name, server) in clusters {
            s += &format!("- name: {name}\n  cluster:\n    server: {server}\n");
        }
        s += "users:\n";
        for (_, _, user) in contexts {
            s += &format!("- name: {user}\n  user:\n    token: t\n");
        }
        s += "contexts:\n";
        for (name, cluster, user) in contexts {
            s += &format!("- name: {name}\n  context:\n    cluster: {cluster}\n    user: {user}\n");
        }
        s
    }

    // covers UP-01, UP-02
    #[test]
    fn kubeconfig_env_replaces_default() {
        let home = Path::new("/home/me");
        assert_eq!(
            kubeconfig_paths(Some(OsStr::new("/a:/b::~/c:/a")), Some(home), &[]),
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/home/me/c")
            ]
        );
        assert_eq!(
            kubeconfig_paths(None, Some(home), &[PathBuf::from("/extra")]),
            vec![
                PathBuf::from("/home/me/.kube/config"),
                PathBuf::from("/extra")
            ]
        );
        assert_eq!(
            kubeconfig_paths(Some(OsStr::new("")), Some(home), &[]),
            vec![PathBuf::from("/home/me/.kube/config")]
        );
    }

    // covers UP-01
    #[test]
    fn merges_files_first_wins_and_tracks_sources() {
        let dir = tempfile::tempdir().unwrap();
        let first = write(
            dir.path(),
            "first",
            &config(
                &[("dev", "dev-cluster", "dev-user"), ("shared", "c1", "u1")],
                &[
                    ("dev-cluster", "https://dev.example:6443"),
                    ("c1", "https://first.example"),
                ],
            ),
        );
        let second = write(
            dir.path(),
            "second",
            &config(
                &[("shared", "c1", "u1"), ("prod", "c1", "u2")],
                // `c1` is redefined here; the first file's server must win.
                &[("c1", "https://second.example")],
            ),
        );
        let missing = dir.path().join("missing");
        let loaded = load(&[first.clone(), missing, second.clone()]);

        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        let names: Vec<&str> = loaded.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["dev", "shared", "prod"]);

        let shared = loaded.context("shared").unwrap();
        assert_eq!(shared.source, first);
        assert_eq!(shared.server.as_deref(), Some("https://first.example"));

        let prod = loaded.context("prod").unwrap();
        assert_eq!(prod.source, second);
        assert_eq!(prod.user.as_deref(), Some("u2"));
        assert_eq!(prod.server.as_deref(), Some("https://first.example"));

        let dev = loaded.context("dev").unwrap();
        assert_eq!(dev.server_host(), Some("dev.example:6443"));
        assert_eq!(loaded.merged.contexts.len(), 3);
        assert_eq!(loaded.merged.clusters.len(), 2);
    }

    // covers UP-02
    #[test]
    fn non_utf8_kubeconfig_env_is_used_not_ignored() {
        use std::os::unix::ffi::OsStrExt;
        let raw = OsStr::from_bytes(b"/tmp/\xffscratch:/b");
        let paths = kubeconfig_paths(Some(raw), Some(Path::new("/home/me")), &[]);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].as_os_str().as_bytes(), b"/tmp/\xffscratch");
        assert!(!paths.contains(&PathBuf::from("/home/me/.kube/config")));
    }

    // covers UP-01
    #[test]
    fn duplicate_names_within_a_file_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "dups",
            "apiVersion: v1\nkind: Config\nclusters:\n- name: a\n  cluster:\n    server: https://a\n- name: b\n  cluster:\n    server: https://b\n- name: a\n  cluster:\n    server: https://a2\nusers:\n- name: u\n  user:\n    token: t\ncontexts:\n- name: dev\n  context:\n    cluster: a\n    user: u\n- name: dev\n  context:\n    cluster: b\n    user: u\n",
        );
        let loaded = load(&[path]);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.contexts.len(), 1);
        assert_eq!(loaded.merged.contexts.len(), 1);
        assert_eq!(loaded.merged.clusters.len(), 2);
        let dev = loaded.context("dev").unwrap();
        assert_eq!(dev.server.as_deref(), Some("https://a"));
    }

    // covers UP-02
    #[test]
    fn kind_and_api_version_mismatch_does_not_drop_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let weird = write(
            dir.path(),
            "weird",
            "apiVersion: v2\nkind: NotConfig\nclusters:\n- name: w\n  cluster:\n    server: https://w\nusers:\n- name: wu\n  user:\n    token: t\ncontexts:\n- name: weird\n  context:\n    cluster: w\n    user: wu\n",
        );
        let good = write(
            dir.path(),
            "good",
            &config(&[("ok", "c", "u")], &[("c", "https://ok")]),
        );
        for order in [[weird.clone(), good.clone()], [good.clone(), weird.clone()]] {
            let loaded = load(&order);
            assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
            let mut names: Vec<&str> = loaded.contexts.iter().map(|c| c.name.as_str()).collect();
            names.sort();
            assert_eq!(names, ["ok", "weird"]);
        }
    }

    // covers UP-02
    #[test]
    fn merge_error_drops_only_the_failing_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(
            dir.path(),
            "a",
            &config(&[("a", "c", "u")], &[("c", "https://a")]),
        );
        let b = write(
            dir.path(),
            "b",
            &config(&[("b", "c", "u")], &[("c", "https://b")]),
        );
        let loaded = load_without(
            &[a.clone(), b.clone()],
            &b,
            vec![(b.clone(), "boom".into())],
        );
        assert_eq!(loaded.contexts.len(), 1);
        assert_eq!(loaded.contexts[0].name, "a");
        assert_eq!(loaded.errors, vec![(b.clone(), "boom".to_owned())]);
        assert_eq!(loaded.paths, vec![a, b]);
    }

    // covers UP-12
    #[test]
    fn file_stamps_change_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        let paths = vec![path.clone()];
        let missing = FileStamps::of(&paths);
        std::fs::write(&path, "a").unwrap();
        let one = FileStamps::of(&paths);
        assert_ne!(missing, one);
        assert_eq!(one, FileStamps::of(&paths));
        std::fs::write(&path, "abc").unwrap();
        assert_ne!(one, FileStamps::of(&paths));
    }

    // covers UP-02
    #[test]
    fn error_summary_is_one_line_without_file_content() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(
            dir.path(),
            "bad",
            "apiVersion: v1\nusers:\n- name: u\n  user:\n    token: SECRETTOKEN [oops\n  - x: [\n",
        );
        let loaded = load(&[bad]);
        let full = &loaded.errors[0].1;
        let summary = error_summary(full);
        assert!(!summary.contains('\n'));
        assert!(!summary.is_empty());
        assert!(!summary.contains("SECRETTOKEN"), "{summary}");
        assert_eq!(error_summary("\n  first\nsecond"), "first");
        assert_eq!(error_summary(&"x".repeat(300)).chars().count(), 201);
    }

    // covers UP-02
    #[test]
    fn invalid_file_is_reported_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(dir.path(), "bad", "this: [is not: valid");
        let good = write(
            dir.path(),
            "good",
            &config(&[("ok", "c", "u")], &[("c", "https://ok")]),
        );
        let loaded = load(&[bad.clone(), good]);
        assert_eq!(loaded.contexts.len(), 1);
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].0, bad);
    }

    // covers UP-01
    #[test]
    fn context_with_missing_cluster_has_no_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "c", &config(&[("orphan", "nowhere", "u")], &[]));
        let loaded = load(&[path]);
        let orphan = loaded.context("orphan").unwrap();
        assert_eq!(orphan.server, None);
        assert_eq!(orphan.server_host(), None);
    }
}
