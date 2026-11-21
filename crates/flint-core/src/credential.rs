//! The credential a reviewed write goes out with.
//!
//! A review binds to its target, its connection definition and, here, its credential. Before
//! every request of a write, [`WriteCredential::present`] works out a [`CredentialBinding`], and
//! the write gate refuses any request whose binding is not the reviewed one.
//!
//! What is compared is also what is sent:
//!
//! - **Token file.** The file is read once for each request. That one read is digested and is the
//!   `Authorization` header of the request, so the client never reads it again in between.
//! - **Certificate and key files.** They are read once when connecting, and the client is built
//!   from those bytes. The binding holds their digest and the digest of the files as they are now,
//!   so a file replaced while a confirmation is open refuses the write.
//! - **Exec plugins and auth providers.** Their output may rotate at any time, so what is bound
//!   is who the server says the credential is (`SelfSubjectReview`). A refreshing token is taken
//!   once for each request, and that same value is sent for both the identity check and the
//!   request.
//!
//! Digests are keyed with a random key that lives in this process's memory only, so one is of no
//! use outside it. Nothing here logs, shows or stores a credential or a digest.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use base64::Engine as _;
use http::header::AUTHORIZATION;
use http::{HeaderValue, Request};
use k8s_openapi::api::authentication::v1::SelfSubjectReview;
use kube::api::{Api, PostParams};
use kube::config::{AuthInfo, Kubeconfig};
use kube::{Client, Error};
use ring::hmac;
use secrecy::SecretString;

use crate::api_error::ApiError;
use crate::auth_plugin::RefreshingToken;
use crate::write::WriteError;

/// The most a credential file may hold.
const FILE_CAP: u64 = 1024 * 1024;
/// How long the identity check of one request may take.
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(30);

/// This process's digest key. `None` if the system has no randomness to give: every write is
/// refused then.
static DIGEST_KEY: LazyLock<Option<hmac::Key>> =
    LazyLock::new(|| hmac::Key::generate(hmac::HMAC_SHA256, &ring::rand::SystemRandom::new()).ok());

/// A keyed digest. It has no `Debug` of its own, so it cannot reach a log through a struct.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Digest([u8; 32]);

/// A digest being computed over several parts, each framed by its length.
struct Digester(hmac::Context);

impl Digester {
    fn new() -> Option<Self> {
        DIGEST_KEY
            .as_ref()
            .map(|key| Digester(hmac::Context::with_key(key)))
    }

    fn add(&mut self, part: &[u8]) {
        self.0.update(&(part.len() as u64).to_le_bytes());
        self.0.update(part);
    }

    fn finish(self) -> Option<Digest> {
        let tag = self.0.sign();
        <[u8; 32]>::try_from(tag.as_ref()).ok().map(Digest)
    }
}

fn digest_of(bytes: &[u8]) -> Option<Digest> {
    let mut digester = Digester::new()?;
    digester.add(bytes);
    digester.finish()
}

/// What a request's credential was when it was checked. Two are equal only if the credential
/// is the same.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CredentialBinding {
    /// Over every credential file: as the client holds it, and as it is now.
    material: Digest,
    /// Who the server says the credential is, when the credential comes from a plugin.
    identity: Option<Digest>,
}

// By hand: a digest stays out of every log.
impl std::fmt::Debug for CredentialBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialBinding").finish_non_exhaustive()
    }
}

impl CredentialBinding {
    /// A binding for a test that does not reach the gate.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        CredentialBinding {
            material: Digest([0; 32]),
            identity: None,
        }
    }

    /// Refuses `now` unless it is this binding.
    pub(crate) fn holds(&self, now: &CredentialBinding) -> Result<(), WriteError> {
        if self == now {
            Ok(())
        } else {
            Err(WriteError::CredentialChanged)
        }
    }
}

/// Where a context's credential comes from, in kube's order of precedence (`Auth::try_from`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialSource {
    /// In the kubeconfig entry, or none: the connection definition covers it.
    Definition,
    TokenFile(PathBuf),
    /// An exec plugin or an auth provider.
    Plugin,
}

impl CredentialSource {
    pub(crate) fn of(auth: &AuthInfo) -> Self {
        if auth.auth_provider.is_some() {
            return CredentialSource::Plugin;
        }
        if (auth.username.is_some() && auth.password.is_some()) || auth.token.is_some() {
            return CredentialSource::Definition;
        }
        if let Some(file) = &auth.token_file {
            return CredentialSource::TokenFile(PathBuf::from(file));
        }
        if auth.exec.is_some() {
            CredentialSource::Plugin
        } else {
            CredentialSource::Definition
        }
    }
}

/// A certificate or key file, with the digest of the bytes the client was built from. `None`
/// if they could not be read then: no write goes out on it.
#[derive(Debug, Clone)]
pub(crate) struct HeldFile {
    path: PathBuf,
    held: Option<HeldDigest>,
}

/// [`Digest`] in a [`HeldFile`], which derives `Debug`.
#[derive(Clone, Copy)]
struct HeldDigest(Digest);

impl std::fmt::Debug for HeldDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HeldDigest(..)")
    }
}

/// Reads the certificate and key files of `context` into `kubeconfig`, so the client is built
/// from exactly the bytes digested here. A file that can't be read stays a path, for kube to
/// report as it always did. Blocks on the file system.
pub(crate) fn hold_credential_files(kubeconfig: &mut Kubeconfig, context: &str) -> Vec<HeldFile> {
    let mut files = Vec::new();
    let Some(entry) = kubeconfig
        .contexts
        .iter()
        .find(|c| c.name == context)
        .and_then(|c| c.context.clone())
    else {
        return files;
    };
    let base64 = base64::engine::general_purpose::STANDARD;
    if let Some(cluster) = kubeconfig
        .clusters
        .iter_mut()
        .find(|n| n.name == entry.cluster)
        .and_then(|n| n.cluster.as_mut())
        && cluster.certificate_authority_data.is_none()
        && let Some(path) = cluster.certificate_authority.clone()
        && let Some(bytes) = hold(&mut files, &path)
    {
        cluster.certificate_authority_data = Some(base64.encode(bytes));
        cluster.certificate_authority = None;
    }
    let user = entry.user.as_ref().and_then(|user| {
        kubeconfig
            .auth_infos
            .iter_mut()
            .find(|n| &n.name == user)
            .and_then(|n| n.auth_info.as_mut())
    });
    if let Some(auth) = user {
        if auth.client_certificate_data.is_none()
            && let Some(path) = auth.client_certificate.clone()
            && let Some(bytes) = hold(&mut files, &path)
        {
            auth.client_certificate_data = Some(base64.encode(bytes));
            auth.client_certificate = None;
        }
        if auth.client_key_data.is_none()
            && let Some(path) = auth.client_key.clone()
            && let Some(bytes) = hold(&mut files, &path)
        {
            auth.client_key_data = Some(SecretString::from(base64.encode(bytes)));
            auth.client_key = None;
        }
    }
    files
}

/// Reads `path` and records it in `files`, with a digest if it could be read.
fn hold(files: &mut Vec<HeldFile>, path: &str) -> Option<Vec<u8>> {
    let path = PathBuf::from(path);
    let bytes = read_file(&path).ok();
    let held = bytes.as_deref().and_then(digest_of).map(HeldDigest);
    files.push(HeldFile { path, held });
    bytes.filter(|_| held.is_some())
}

/// Reads a regular file of at most [`FILE_CAP`] bytes.
fn read_file(path: &Path) -> std::io::Result<Vec<u8>> {
    // Follows a symlink, as projected tokens are. A pipe would block the read for good.
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(FILE_CAP + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > FILE_CAP {
        return Err(std::io::ErrorKind::FileTooLarge.into());
    }
    Ok(bytes)
}

/// [`read_file`] off the async runtime's threads.
async fn read_off_thread(path: PathBuf) -> Result<Vec<u8>, WriteError> {
    tokio::task::spawn_blocking(move || read_file(&path))
        .await
        .map_err(|_| WriteError::CredentialUnreadable)?
        .map_err(|_| WriteError::CredentialUnreadable)
}

/// The token a request is sent with, when it is not fixed in the client.
#[derive(Clone)]
enum TokenSource {
    Fixed,
    File(PathBuf),
    Refreshing(RefreshingToken),
}

/// A connection's credential, as its write client presents it.
#[derive(Clone)]
pub(crate) struct WriteCredential(Arc<Parts>);

struct Parts {
    files: Vec<HeldFile>,
    token: TokenSource,
    /// The credential comes from a plugin: its identity is what a review binds.
    check_identity: bool,
}

/// A client for one request, and what its credential was when checked.
pub(crate) struct Presented {
    pub(crate) client: Client,
    pub(crate) binding: CredentialBinding,
}

impl WriteCredential {
    pub(crate) fn new(
        files: Vec<HeldFile>,
        source: CredentialSource,
        refreshing: Option<RefreshingToken>,
    ) -> Self {
        let check_identity = source == CredentialSource::Plugin;
        let token = match (source, refreshing) {
            (CredentialSource::TokenFile(path), _) => TokenSource::File(path),
            (CredentialSource::Plugin, Some(token)) => TokenSource::Refreshing(token),
            _ => TokenSource::Fixed,
        };
        WriteCredential(Arc::new(Parts {
            files,
            token,
            check_identity,
        }))
    }

    /// Reads the credential for one request, and returns `client` set to send exactly that.
    /// Fails closed: anything that can't be read or confirmed refuses the request.
    pub(crate) async fn present(&self, client: Client) -> Result<Presented, WriteError> {
        let parts = &self.0;
        let mut material = Digester::new().ok_or(WriteError::CredentialUnreadable)?;
        for file in &parts.files {
            let held = file.held.ok_or(WriteError::CredentialUnreadable)?;
            let now = read_off_thread(file.path.clone()).await?;
            let now = digest_of(&now).ok_or(WriteError::CredentialUnreadable)?;
            material.add(&held.0.0);
            material.add(&now.0);
        }
        let header = match &parts.token {
            TokenSource::Fixed => None,
            TokenSource::File(path) => {
                let bytes = read_off_thread(path.clone()).await?;
                material.add(&bytes);
                Some(file_token_header(bytes)?)
            }
            TokenSource::Refreshing(token) => Some(
                token
                    .header()
                    .await
                    .map_err(|_| WriteError::IdentityUnverified)?,
            ),
        };
        let client = match header {
            Some(header) => crate::cluster::sending_credential(client, BoundHeader(header)),
            None => client,
        };
        let identity = if parts.check_identity {
            Some(authenticated_identity(&client).await?)
        } else {
            None
        };
        Ok(Presented {
            client,
            binding: CredentialBinding {
                material: material.finish().ok_or(WriteError::CredentialUnreadable)?,
                identity,
            },
        })
    }
}

/// The header for a token file's contents, as kube builds it: the text as it is.
fn file_token_header(bytes: Vec<u8>) -> Result<HeaderValue, WriteError> {
    let token = String::from_utf8(bytes).map_err(|_| WriteError::CredentialUnreadable)?;
    let mut value = HeaderValue::try_from(format!("Bearer {token}"))
        .map_err(|_| WriteError::CredentialUnreadable)?;
    value.set_sensitive(true);
    Ok(value)
}

/// The `Authorization` a request must go out with, whatever the client's own auth would set.
#[derive(Clone)]
pub(crate) struct BoundHeader(pub(crate) HeaderValue);

/// The step of the client stack, below its auth, that applies a [`BoundHeader`].
pub(crate) fn apply_bound_header<B>(mut request: Request<B>) -> Request<B> {
    if let Some(BoundHeader(value)) = request.extensions().get::<BoundHeader>().cloned() {
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    request
}

/// Who the server says `client`'s credential is, as a digest.
async fn authenticated_identity(client: &Client) -> Result<Digest, WriteError> {
    let api: Api<SelfSubjectReview> = Api::all(client.clone());
    let review = tokio::time::timeout(
        IDENTITY_TIMEOUT,
        api.create(&PostParams::default(), &SelfSubjectReview::default()),
    )
    .await
    .map_err(|_| WriteError::IdentityUnverified)?
    .map_err(|error| identity_error(&error))?;
    let user = review
        .status
        .and_then(|status| status.user_info)
        .ok_or(WriteError::IdentityUnverified)?;
    let username = user
        .username
        .filter(|name| !name.is_empty())
        .ok_or(WriteError::IdentityUnverified)?;
    let mut groups = user.groups.unwrap_or_default();
    groups.sort();
    let extra: BTreeMap<_, _> = user.extra.unwrap_or_default();
    let mut digester = Digester::new().ok_or(WriteError::IdentityUnverified)?;
    digester.add(username.as_bytes());
    digester.add(user.uid.unwrap_or_default().as_bytes());
    digester.add(&(groups.len() as u64).to_le_bytes());
    for group in &groups {
        digester.add(group.as_bytes());
    }
    digester.add(&(extra.len() as u64).to_le_bytes());
    for (key, values) in extra {
        let mut values = values;
        values.sort();
        digester.add(key.as_bytes());
        digester.add(&(values.len() as u64).to_le_bytes());
        for value in &values {
            digester.add(value.as_bytes());
        }
    }
    digester.finish().ok_or(WriteError::IdentityUnverified)
}

/// A server that does not know the API answers 404, or 405 on a path it routes otherwise.
fn identity_error(error: &Error) -> WriteError {
    match ApiError::from_kube(error).map(|status| status.code) {
        Some(404 | 405) => WriteError::IdentityUnavailable,
        _ => WriteError::IdentityUnverified,
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Mutex;

    use http::Response;
    use kube::client::Body;
    use tower::ServiceBuilder;

    use super::*;

    type Seen = Arc<Mutex<Vec<String>>>;

    /// A client whose API answers a `SelfSubjectReview` with `answer(authorization)`, and keeps
    /// the `Authorization` of every request.
    #[allow(clippy::disallowed_methods)] // No cluster and no credentials: a stub transport.
    fn api(answer: impl Fn(&str) -> (u16, String) + Send + Clone + 'static) -> (Client, Seen) {
        let seen = Seen::default();
        let kept = seen.clone();
        let service = ServiceBuilder::new()
            .map_request(apply_bound_header::<Body>)
            .service(tower::service_fn(move |request: Request<Body>| {
                let authorization = request
                    .headers()
                    .get(AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                kept.lock().unwrap().push(authorization.clone());
                let (code, body) = answer(&authorization);
                std::future::ready(Ok::<_, Infallible>(
                    Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(Body::from(body.into_bytes()))
                        .unwrap(),
                ))
            }));
        (Client::new(service, "default"), seen)
    }

    fn review_of(name: &str) -> (u16, String) {
        let body = serde_json::json!({
            "apiVersion": "authentication.k8s.io/v1",
            "kind": "SelfSubjectReview",
            "metadata": {},
            "status": {"userInfo": {"username": name, "groups": ["b", "a"]}},
        });
        (201, body.to_string())
    }

    fn status(code: u16) -> (u16, String) {
        let body = serde_json::json!({
            "apiVersion": "v1", "kind": "Status", "metadata": {},
            "status": "Failure", "reason": "Refused", "message": "refused", "code": code,
        });
        (code, body.to_string())
    }

    fn token_file(path: &Path) -> WriteCredential {
        let source = CredentialSource::TokenFile(path.to_owned());
        WriteCredential::new(Vec::new(), source, None)
    }

    fn plugin() -> WriteCredential {
        WriteCredential::new(Vec::new(), CredentialSource::Plugin, None)
    }

    #[tokio::test]
    async fn a_token_file_is_sent_as_it_was_digested() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "first").unwrap();
        let (client, seen) = api(|_| review_of("someone"));
        let credential = token_file(&path);

        let first = credential.present(client.clone()).await.unwrap();
        let again = credential.present(client.clone()).await.unwrap();
        first.binding.holds(&again.binding).unwrap();

        // Rewritten once the request has its credential: the request still sends what was read.
        std::fs::write(&path, "second").unwrap();
        let _: SelfSubjectReview = Api::all(again.client)
            .create(&PostParams::default(), &SelfSubjectReview::default())
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().as_slice(), ["Bearer first"]);

        let changed = credential.present(client).await.unwrap();
        assert_eq!(
            first.binding.holds(&changed.binding),
            Err(WriteError::CredentialChanged)
        );
    }

    #[tokio::test]
    async fn an_unreadable_token_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (client, _) = api(|_| review_of("someone"));
        for path in [dir.path().join("missing"), dir.path().to_owned()] {
            let refused = token_file(&path).present(client.clone()).await;
            assert_eq!(
                refused.err().map(|e| e.to_string()),
                Some(WriteError::CredentialUnreadable.to_string())
            );
        }
        let binary = dir.path().join("binary");
        std::fs::write(&binary, [0xff, 0xfe]).unwrap();
        let refused = token_file(&binary).present(client).await;
        assert!(matches!(refused, Err(WriteError::CredentialUnreadable)));
    }

    #[tokio::test]
    async fn another_identity_is_another_binding() {
        let (alice, _) = api(|_| review_of("alice"));
        let (alice_again, _) = api(|_| review_of("alice"));
        let (bob, _) = api(|_| review_of("bob"));
        let credential = plugin();
        let reviewed = credential.present(alice).await.unwrap().binding;
        let same = credential.present(alice_again).await.unwrap().binding;
        let other = credential.present(bob).await.unwrap().binding;
        reviewed.holds(&same).unwrap();
        assert_eq!(reviewed.holds(&other), Err(WriteError::CredentialChanged));
    }

    #[tokio::test]
    async fn an_identity_that_cannot_be_confirmed_is_refused() {
        for (code, expected) in [
            (404, WriteError::IdentityUnavailable),
            (405, WriteError::IdentityUnavailable),
            (403, WriteError::IdentityUnverified),
            (500, WriteError::IdentityUnverified),
        ] {
            let (client, _) = api(move |_| status(code));
            let refused = plugin().present(client).await;
            assert_eq!(refused.err(), Some(expected), "{code}");
        }
        let (client, _) = api(|_| {
            let body = serde_json::json!({
                "apiVersion": "authentication.k8s.io/v1",
                "kind": "SelfSubjectReview",
                "metadata": {},
            });
            (201, body.to_string())
        });
        let refused = plugin().present(client).await;
        assert_eq!(refused.err(), Some(WriteError::IdentityUnverified));
    }

    #[test]
    fn the_source_follows_the_order_of_precedence() {
        let file = AuthInfo {
            token_file: Some("/token".into()),
            ..AuthInfo::default()
        };
        assert_eq!(
            CredentialSource::of(&file),
            CredentialSource::TokenFile("/token".into())
        );
        let inline = AuthInfo {
            token: Some(SecretString::from("t")),
            ..file
        };
        assert_eq!(CredentialSource::of(&inline), CredentialSource::Definition);
        let exec: kube::config::ExecConfig = serde_json::from_value(serde_json::json!({
            "apiVersion": "client.authentication.k8s.io/v1",
            "command": "plugin",
        }))
        .unwrap();
        let plugin = AuthInfo {
            exec: Some(exec),
            ..AuthInfo::default()
        };
        assert_eq!(CredentialSource::of(&plugin), CredentialSource::Plugin);
        assert_eq!(
            CredentialSource::of(&AuthInfo::default()),
            CredentialSource::Definition
        );
    }

    #[test]
    fn held_files_replace_their_paths_with_the_bytes_read() {
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca");
        std::fs::write(&ca, "authority").unwrap();
        let missing = dir.path().join("missing");
        let mut kubeconfig = Kubeconfig::from_yaml(&format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: \
             https://a\n    certificate-authority: {}\nusers:\n- name: u\n  user:\n    \
             client-certificate: {}\n    client-key: {}\ncontexts:\n- name: ctx\n  context:\n    \
             cluster: c\n    user: u\n",
            ca.display(),
            missing.display(),
            missing.display(),
        ))
        .unwrap();
        let files = hold_credential_files(&mut kubeconfig, "ctx");
        assert_eq!(files.len(), 3);
        assert!(files[0].held.is_some());
        assert!(files[1].held.is_none(), "unreadable: nothing is held");

        let cluster = kubeconfig.clusters[0].cluster.as_ref().unwrap();
        assert_eq!(cluster.certificate_authority, None);
        let data = cluster.certificate_authority_data.as_deref().unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        assert_eq!(decoded, b"authority");
        let auth = kubeconfig.auth_infos[0].auth_info.as_ref().unwrap();
        assert!(auth.client_certificate.is_some(), "left for kube to report");
    }

    #[test]
    fn a_binding_prints_nothing_of_its_digests() {
        let binding = CredentialBinding {
            material: Digest([7; 32]),
            identity: Some(Digest([9; 32])),
        };
        assert_eq!(format!("{binding:?}"), "CredentialBinding { .. }");
    }
}
