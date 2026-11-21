//! How one server differs from the recording: its `/version`, the resources it lacks, and the
//! control of its clock. Together they let a test run a set of unlike clusters.

use super::*;

/// Fails on a hidden resource the fixtures do not serve: a typo would hide nothing.
pub(super) fn check_hidden(fixtures: &Fixtures, hidden: &[Gvr]) -> Result<()> {
    for gvr in hidden {
        if fixtures.resources.get(gvr).is_none() {
            bail!("Options::hidden names {gvr}, which the fixtures do not serve");
        }
    }
    Ok(())
}

/// What `/version` answers. An override replaces `gitVersion`, `major` and `minor`, and drops the
/// compatibility fields, which older servers do not send and which would contradict it.
pub(super) fn version(recorded: &Value, git_version: Option<&str>) -> Result<Value> {
    let Some(git_version) = git_version else {
        return Ok(recorded.clone());
    };
    let (major, minor) = major_minor(git_version).with_context(|| {
        format!("Options::version {git_version:?} is not v<major>.<minor>.<patch>")
    })?;
    let mut version = recorded.clone();
    let Some(fields) = version.as_object_mut() else {
        bail!("the recorded /version is not an object");
    };
    for field in [
        "emulationMajor",
        "emulationMinor",
        "minCompatibilityMajor",
        "minCompatibilityMinor",
    ] {
        fields.remove(field);
    }
    fields.insert("gitVersion".into(), git_version.into());
    fields.insert("major".into(), major.into());
    fields.insert("minor".into(), minor.into());
    Ok(version)
}

/// The major and minor of `v1.31.4`, with or without a build suffix (`-eks-1a2b3c`, `+k3s1`).
fn major_minor(git_version: &str) -> Option<(&str, &str)> {
    let mut parts = git_version.strip_prefix('v')?.splitn(3, '.');
    let (major, minor, patch) = (parts.next()?, parts.next()?, parts.next()?);
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let patch = patch.split(['-', '+']).next()?;
    (number(major) && number(minor) && number(patch)).then_some((major, minor))
}

/// Removes hidden resources from an aggregated discovery list, and any version or group left
/// with none. Versions and groups that lost nothing stay as recorded.
pub(super) fn hide_from_discovery(discovery: &mut Value, hidden: &[Gvr]) {
    if hidden.is_empty() {
        return;
    }
    let Some(groups) = discovery.get_mut("items").and_then(Value::as_array_mut) else {
        return;
    };
    groups.retain_mut(|group| {
        let name = text_at(group, "/metadata/name");
        let Some(versions) = group.get_mut("versions").and_then(Value::as_array_mut) else {
            return true;
        };
        let recorded = versions.len();
        versions.retain_mut(|version| {
            let number = text_at(version, "/version");
            let Some(resources) = version.get_mut("resources").and_then(Value::as_array_mut) else {
                return true;
            };
            let listed = resources.len();
            resources
                .retain(|r| !hidden.contains(&Gvr::new(&name, &number, &text_at(r, "/resource"))));
            resources.len() == listed || !resources.is_empty()
        });
        versions.len() == recorded || !versions.is_empty()
    });
}

fn text_at(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Whether the OpenAPI document at `path` (`apis/apps/v1`, `api/v1`) has only hidden resources.
pub(super) fn openapi_hidden(fixtures: &Fixtures, hidden: &[Gvr], path: &str) -> bool {
    if hidden.is_empty() {
        return false;
    }
    let segments: Vec<&str> = path.split('/').collect();
    let (group, version) = match segments.as_slice() {
        ["api", version] => ("", *version),
        ["apis", group, version] => (*group, *version),
        _ => return false,
    };
    let mut served = fixtures
        .resources
        .iter()
        .filter(|r| r.gvr.group == group && r.gvr.version == version)
        .peekable();
    served.peek().is_some() && served.all(|r| hidden.contains(&r.gvr))
}

/// The OpenAPI index without the documents [`openapi_hidden`] removes.
pub(super) fn openapi_index(fixtures: &Fixtures, hidden: &[Gvr]) -> Value {
    let mut index = fixtures.openapi_index.clone();
    if let Some(paths) = index.get_mut("paths").and_then(Value::as_object_mut) {
        paths.retain(|path, _| !openapi_hidden(fixtures, hidden, path));
    }
    index
}

/// Removes the CustomResourceDefinition of each custom resource hidden in all its versions. One
/// with a version still served keeps its definition.
pub(super) fn remove_hidden_crds(state: &mut State, fixtures: &Fixtures, hidden: &[Gvr]) {
    let crds = Gvr::new("apiextensions.k8s.io", "v1", "customresourcedefinitions");
    if fixtures.resources.get(&crds).is_none() {
        return;
    }
    for gvr in hidden.iter().filter(|gvr| !gvr.group.is_empty()) {
        let every_version_hidden = fixtures
            .resources
            .iter()
            .filter(|r| r.gvr.group == gvr.group && r.gvr.resource == gvr.resource)
            .all(|r| hidden.contains(&r.gvr));
        if every_version_hidden {
            state.silent_remove(&crds, None, &format!("{}.{}", gvr.resource, gvr.group));
        }
    }
}

impl FakeApiServer {
    /// Moves the server's time forward: ages, new timestamps and log times follow, on the next
    /// response. Stored timestamps do not move, so everything looks older.
    pub fn advance_clock(&self, by: Duration) {
        self.inner.clock.advance(by);
    }

    /// Stops the server's time until [`FakeApiServer::resume_clock`], so ages and new timestamps
    /// are exact. [`FakeApiServer::advance_clock`] still moves it.
    pub fn freeze_clock(&self) {
        self.inner.clock.freeze();
    }

    /// Starts a frozen clock again from the time it shows.
    pub fn resume_clock(&self) {
        self.inner.clock.resume();
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use serde_json::json;

    use super::*;
    use crate::fake_api::clock;

    const TABLE: &str = "application/json;as=Table;v=v1;g=meta.k8s.io, application/json";
    const DISCOVERY: &str = "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList";
    const POD: &str = "web-69c6f74b8b-qv4xv";

    async fn get(client: &kube::Client, path: &str, accept: &str) -> Result<Value, kube::Error> {
        let request = http::Request::get(path)
            .header(http::header::ACCEPT, accept)
            .body(Vec::new())
            .unwrap();
        client.request::<Value>(request).await
    }

    async fn code(client: &kube::Client, request: http::Request<Vec<u8>>) -> u16 {
        match client.request::<Value>(request).await {
            Ok(_) => 200,
            Err(kube::Error::Api(status)) => status.code,
            Err(other) => panic!("not an API error: {other}"),
        }
    }

    async fn get_code(client: &kube::Client, path: &str) -> u16 {
        let request = http::Request::get(path)
            .header(http::header::ACCEPT, TABLE)
            .body(Vec::new())
            .unwrap();
        code(client, request).await
    }

    fn listed(discovery: &Value) -> Vec<String> {
        let mut names = Vec::new();
        for group in discovery["items"].as_array().unwrap() {
            for version in group["versions"].as_array().unwrap() {
                for resource in version["resources"].as_array().unwrap() {
                    names.push(format!(
                        "{}/{}/{}",
                        text_at(group, "/metadata/name"),
                        text_at(version, "/version"),
                        text_at(resource, "/resource")
                    ));
                }
            }
        }
        names
    }

    async fn hiding(hidden: Vec<Gvr>) -> FakeApiServer {
        FakeApiServer::start_with(Options {
            hidden,
            ..Options::default()
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn defaults_answer_as_recorded() {
        let server = FakeApiServer::start().await.unwrap();
        let client = server.unrestricted_client().await.unwrap();
        let fixtures = server.fixtures();
        assert_eq!(
            get(&client, "/version", "application/json").await.unwrap(),
            fixtures.version
        );
        assert_eq!(
            get(&client, "/api", DISCOVERY).await.unwrap(),
            fixtures.discovery_core
        );
        assert_eq!(
            get(&client, "/apis", DISCOVERY).await.unwrap(),
            fixtures.discovery_groups
        );
        assert_eq!(
            get(&client, "/openapi/v3", "application/json")
                .await
                .unwrap(),
            fixtures.openapi_index
        );
    }

    #[tokio::test]
    async fn overrides_the_version_per_server() {
        let old = FakeApiServer::start_with(Options {
            version: Some("v1.31.4-eks-1a2b3c".into()),
            ..Options::default()
        })
        .await
        .unwrap();
        let recorded = FakeApiServer::start().await.unwrap();
        let client = old.unrestricted_client().await.unwrap();
        let info = get(&client, "/version", "application/json").await.unwrap();
        assert_eq!(info["gitVersion"], "v1.31.4-eks-1a2b3c");
        assert_eq!(
            (&info["major"], &info["minor"]),
            (&json!("1"), &json!("31"))
        );
        assert!(info.get("emulationMinor").is_none(), "{info}");
        assert_eq!(info["platform"], recorded.fixtures().version["platform"]);
        let client = recorded.unrestricted_client().await.unwrap();
        let info = get(&client, "/version", "application/json").await.unwrap();
        assert_eq!(info, recorded.fixtures().version);
    }

    #[tokio::test]
    async fn refuses_a_malformed_version_and_an_unknown_hidden_resource() {
        for bad in ["1.31.4", "v1.31", "v1.x.4", "v1.31.x", ""] {
            let started = FakeApiServer::start_with(Options {
                version: Some(bad.into()),
                ..Options::default()
            })
            .await;
            assert!(started.is_err(), "{bad:?} was accepted");
        }
        for good in ["v1.31.4", "v1.31.4+k3s1", "v1.31.4-gke.100"] {
            assert!(major_minor(good).is_some(), "{good}");
        }
        let started = FakeApiServer::start_with(Options {
            hidden: vec![Gvr::new("apps", "v1", "deploymentz")],
            ..Options::default()
        })
        .await;
        let err = started.err().expect("a typo fails the start").to_string();
        assert!(err.contains("deploymentz"), "{err}");
    }

    #[tokio::test]
    async fn a_hidden_resource_is_gone_from_discovery_and_every_route() {
        let pods = Gvr::core("pods");
        let server = hiding(vec![pods.clone()]).await;
        let client = server.unrestricted_client().await.unwrap();
        let core = listed(&get(&client, "/api", DISCOVERY).await.unwrap());
        assert!(!core.contains(&"/v1/pods".to_owned()), "{core:?}");
        assert!(core.contains(&"/v1/services".to_owned()), "{core:?}");

        let object = format!("/api/v1/namespaces/demo/pods/{POD}");
        for path in [
            "/api/v1/pods".to_owned(),
            "/api/v1/namespaces/demo/pods".to_owned(),
            "/api/v1/pods?watch=true".to_owned(),
            object.clone(),
            format!("{object}/log"),
        ] {
            assert_eq!(get_code(&client, &path).await, 404, "{path}");
        }
        let delete = http::Request::delete(&object).body(Vec::new()).unwrap();
        assert_eq!(code(&client, delete).await, 404);
        assert_eq!(server.writes().last().unwrap().status, Some(404));
        assert_eq!(get_code(&client, "/api/v1/services").await, 200);
    }

    #[tokio::test]
    async fn hiding_a_whole_group_version_drops_it_and_its_openapi_document() {
        let fixtures = Fixtures::shared_default().unwrap();
        let apps: Vec<Gvr> = fixtures
            .resources
            .iter()
            .filter(|r| r.gvr.group == "apps" && r.gvr.version == "v1")
            .map(|r| r.gvr.clone())
            .collect();
        let (last, some) = apps.split_last().unwrap();
        let partial = hiding(some.to_vec()).await;
        let client = partial.unrestricted_client().await.unwrap();
        let index = get(&client, "/openapi/v3", "application/json")
            .await
            .unwrap();
        assert!(index["paths"].get("apis/apps/v1").is_some(), "{index}");
        let groups = listed(&get(&client, "/apis", DISCOVERY).await.unwrap());
        assert!(
            groups.contains(&format!("apps/v1/{}", last.resource)),
            "{groups:?}"
        );

        let whole = hiding(apps.clone()).await;
        let client = whole.unrestricted_client().await.unwrap();
        let discovery = get(&client, "/apis", DISCOVERY).await.unwrap();
        let names: Vec<_> = discovery["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| text_at(g, "/metadata/name"))
            .collect();
        assert!(!names.contains(&"apps".to_owned()), "{names:?}");
        let index = get(&client, "/openapi/v3", "application/json")
            .await
            .unwrap();
        assert!(index["paths"].get("apis/apps/v1").is_none(), "{index}");
        assert_eq!(get_code(&client, "/openapi/v3/apis/apps/v1").await, 404);
    }

    #[tokio::test]
    async fn hiding_a_crd_removes_its_definition_too() {
        let fixtures = Fixtures::shared_default().unwrap();
        let gadgets = fixtures.resources.by_plural("gadgets").unwrap().gvr.clone();
        let crds = Gvr::new("apiextensions.k8s.io", "v1", "customresourcedefinitions");
        let name = format!("gadgets.{}", gadgets.group);
        let server = hiding(vec![gadgets.clone()]).await;
        assert!(server.object(&crds, None, &name).is_none());
        let client = server.unrestricted_client().await.unwrap();
        let path = gadgets.collection_path(None);
        assert_eq!(get_code(&client, &path).await, 404);

        // Another server on the same shared fixtures still has it.
        let other = FakeApiServer::start().await.unwrap();
        assert!(other.object(&crds, None, &name).is_some());
        let client = other.unrestricted_client().await.unwrap();
        assert_eq!(get_code(&client, &path).await, 200);
    }

    #[test]
    fn hiding_keeps_a_group_with_another_version() {
        let mut discovery = json!({"items": [{
            "metadata": {"name": "example.dev"},
            "versions": [
                {"version": "v2", "resources": [{"resource": "widgets"}]},
                {"version": "v1", "resources": [{"resource": "widgets"}]},
            ],
        }]});
        hide_from_discovery(&mut discovery, &[Gvr::new("example.dev", "v2", "widgets")]);
        assert_eq!(listed(&discovery), ["example.dev/v1/widgets"]);
    }

    #[tokio::test]
    #[should_panic(expected = "Options::hidden")]
    async fn using_a_hidden_resource_is_a_test_bug() {
        let pods = Gvr::core("pods");
        let server = hiding(vec![pods.clone()]).await;
        server.rows(&pods);
    }

    #[tokio::test]
    async fn the_clock_moves_ages_and_new_timestamps() {
        let start: i64 = 1_900_000_000;
        let server = FakeApiServer::start_with(Options {
            now: UNIX_EPOCH + Duration::from_secs(start.unsigned_abs()),
            ..Options::default()
        })
        .await
        .unwrap();
        let client = server.unrestricted_client().await.unwrap();
        let namespaces = Gvr::core("namespaces");
        let age = server.column(&namespaces, "Age").unwrap();
        let stored = server.object(&namespaces, None, "demo").unwrap();
        // Objects are stored in fixture time.
        let created = stored["metadata"]["creationTimestamp"]
            .as_str()
            .and_then(clock::parse)
            .unwrap()
            .0;
        let recorded_age = clock::FIXTURE_NOW_SECONDS - created;

        server.freeze_clock();
        let advance: i64 = 30 * 86_400;
        server.advance_clock(Duration::from_secs(advance.unsigned_abs()));
        let table = get(&client, "/api/v1/namespaces/demo", TABLE)
            .await
            .unwrap();
        assert_eq!(
            table["rows"][0]["cells"][age],
            flint_core::table::human_duration(recorded_age + advance)
        );
        let object = get(&client, "/api/v1/namespaces/demo", "application/json")
            .await
            .unwrap();
        assert_eq!(
            object["metadata"]["creationTimestamp"],
            clock::format(start - recorded_age, 0),
            "stored timestamps do not move"
        );

        let now = server.inner.clock.now();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(server.inner.clock.now(), now, "frozen");
        server.resume_clock();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(server.inner.clock.now() > now, "running again");
    }
}
