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
//!   is who the server says the credential is (`SelfSubjectReview`). A refreshing token or an
//!   auth provider's header is taken once for each request, and that same value is sent for both
//!   the identity check and the request, past the client's own auth, which would resolve it again.
//!
//! Digests are keyed with a random key that lives in this process's memory only, so one is of no
//! use outside it. Nothing here logs, shows or stores a credential or a digest.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use http::header::AUTHORIZATION;
use http::{HeaderValue, Request};
use k8s_openapi::api::authentication::v1::SelfSubjectReview;
use kube::api::{Api, PostParams};
use kube::client::Body;
use kube::client::middleware::AuthLayer;
use kube::config::{AuthInfo, Kubeconfig};
use kube::{Client, Error};
use ring::hmac;
use secrecy::SecretString;
use tower::{BoxError, Layer as _, Service, ServiceExt as _};

use crate::api_error::ApiError;
use crate::auth_plugin::RefreshingToken;
use crate::connection::policy::{CredentialGrant, CredentialLease};
use crate::write::WriteError;

/// The most a credential file may hold.
const FILE_CAP: u64 = 1024 * 1024;
/// How long the identity check of one request may take.
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(30);
/// An `extra` the server issues anew with each token. It says which token, not who, so a rotated
/// token of the same identity keeps its binding.
const CREDENTIAL_ID: &str = "authentication.kubernetes.io/credential-id";

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
    /// An exec plugin.
    Plugin,
    /// An auth provider, which kube's auth layer resolves.
    AuthProvider,
}

impl CredentialSource {
    pub(crate) fn of(auth: &AuthInfo) -> Self {
        if auth.auth_provider.is_some() {
            return CredentialSource::AuthProvider;
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

    /// Whether using this credential can start a program.
    pub(crate) fn runs_a_command(&self) -> bool {
        matches!(
            self,
            CredentialSource::Plugin | CredentialSource::AuthProvider
        )
    }
}

/// A certificate or key file, with the digest of the bytes the client was built from. `None`
/// if they could not be read then: no write goes out on it.
#[derive(Debug, Clone)]
pub(crate) struct HeldFile {
    path: PathBuf,
    held: Option<HeldDigest>,
    role: FileRole,
}

/// What a held file is to the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileRole {
    Authority,
    /// A client certificate or key, which an exec plugin's own pair replaces.
    ClientIdentity,
}

/// The files of `files` the client was built from, once an exec plugin has run:
/// `plugin_certificate` says its certificate and key replaced the kubeconfig's.
pub(crate) fn files_in_use(files: Vec<HeldFile>, plugin_certificate: bool) -> Vec<HeldFile> {
    files
        .into_iter()
        .filter(|file| !plugin_certificate || file.role != FileRole::ClientIdentity)
        .collect()
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
        && let Some(bytes) = hold(&mut files, &path, FileRole::Authority)
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
            && let Some(bytes) = hold(&mut files, &path, FileRole::ClientIdentity)
        {
            auth.client_certificate_data = Some(base64.encode(bytes));
            auth.client_certificate = None;
        }
        if auth.client_key_data.is_none()
            && let Some(path) = auth.client_key.clone()
            && let Some(bytes) = hold(&mut files, &path, FileRole::ClientIdentity)
        {
            auth.client_key_data = Some(SecretString::from(base64.encode(bytes)));
            auth.client_key = None;
        }
    }
    files
}

/// Reads `path` and records it in `files`, with a digest if it could be read.
fn hold(files: &mut Vec<HeldFile>, path: &str, role: FileRole) -> Option<Vec<u8>> {
    let path = PathBuf::from(path);
    let bytes = read_file(&path).ok();
    let held = bytes.as_deref().and_then(digest_of).map(HeldDigest);
    files.push(HeldFile { path, held, role });
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
    /// Kube's auth layer for an auth provider. `None` if it has none: no write goes out.
    Provider(Option<Arc<AuthLayer>>),
}

/// A connection's credential, as its write client presents it.
#[derive(Clone)]
pub(crate) struct WriteCredential(Arc<Parts>);

struct Parts {
    files: Vec<HeldFile>,
    token: TokenSource,
    /// The credential comes from a plugin: its identity is what a review binds.
    check_identity: bool,
    /// Whether the credential may be used now.
    grant: CredentialGrant,
    /// The provider's layer behind its guard, if a role can stop a command it would run.
    guarded: Option<GuardedProvider>,
    /// What the client read from its token file when it was admitted.
    admitted_token: Option<SecretString>,
}

/// Which connection's credential something was learned with. Holds none of it: the credential
/// is dropped with its connection, and an id never matches a later one.
pub(crate) struct CredentialId(Weak<Parts>);

impl CredentialId {
    pub(crate) fn is(&self, other: &CredentialId) -> bool {
        self.0.ptr_eq(&other.0)
    }
}

/// A client for one request, and what its credential was when checked.
pub(crate) struct Presented {
    pub(crate) client: Client,
    pub(crate) binding: CredentialBinding,
}

impl WriteCredential {
    /// A credential under [`AlwaysAllow`](crate::connection::policy::AlwaysAllow).
    #[cfg(test)]
    pub(crate) fn new(
        files: Vec<HeldFile>,
        source: CredentialSource,
        refreshing: Option<RefreshingToken>,
        auth: Option<Arc<AuthLayer>>,
    ) -> Self {
        let grant = crate::connection::policy::CredentialAccess::always().grant(source.clone());
        Self::with_grant(files, source, refreshing, auth, grant, None)
    }

    pub(crate) fn with_grant(
        files: Vec<HeldFile>,
        source: CredentialSource,
        refreshing: Option<RefreshingToken>,
        auth: Option<Arc<AuthLayer>>,
        grant: CredentialGrant,
        guarded: Option<GuardedProvider>,
    ) -> Self {
        let check_identity = source.runs_a_command();
        let token = match (source, refreshing) {
            (CredentialSource::TokenFile(path), _) => TokenSource::File(path),
            (CredentialSource::AuthProvider, _) => TokenSource::Provider(auth),
            (CredentialSource::Plugin, Some(token)) => TokenSource::Refreshing(token),
            _ => TokenSource::Fixed,
        };
        WriteCredential(Arc::new(Parts {
            files,
            token,
            check_identity,
            grant,
            guarded,
            admitted_token: None,
        }))
    }

    pub(crate) fn id(&self) -> CredentialId {
        CredentialId(Arc::downgrade(&self.0))
    }

    /// The bytes of the certificate or key file at `path`, only while it still holds what this
    /// connection was built from. Blocks on the file system.
    pub(crate) fn held_file_bytes(&self, path: &Path) -> Option<Vec<u8>> {
        let file = self.0.files.iter().find(|file| file.path == path)?;
        let held = file.held?;
        let now = read_file(path).ok()?;
        (digest_of(&now)?.0 == held.0.0).then_some(now)
    }

    /// The token this connection was admitted with, if `path` is its token file.
    pub(crate) fn admitted_token(&self, path: &Path) -> Option<SecretString> {
        match &self.0.token {
            TokenSource::File(file) if file == path => self.0.admitted_token.clone(),
            _ => None,
        }
    }

    /// This credential, with `token` as what its client read from its token file when admitted.
    pub(crate) fn with_admitted_token(mut self, token: Option<SecretString>) -> Self {
        if let Some(parts) = Arc::get_mut(&mut self.0) {
            parts.admitted_token = token;
        }
        self
    }

    /// Whether this connection's credential can start a program, whatever the kubeconfig says now.
    pub(crate) fn runs_a_command(&self) -> bool {
        self.0.grant.runs_a_command()
    }

    /// Whether a server refused this credential in the background since this was last asked.
    pub(crate) fn take_refused(&self) -> bool {
        self.0.grant.take_refused()
    }

    /// Whether this connection can no longer authenticate new requests: its provider's lease
    /// ended, which no later role brings back.
    pub(crate) fn spent(&self) -> bool {
        self.0.guarded.as_ref().is_some_and(GuardedProvider::spent)
    }

    /// Reads the credential for one request, and returns `client` set to send exactly that.
    /// Fails closed: anything that can't be read or confirmed refuses the request, and so does a
    /// credential its grant does not allow, with nothing run or sent.
    pub(crate) async fn present(&self, client: Client) -> Result<Presented, WriteError> {
        let parts = &self.0;
        let lease = parts.grant.lease();
        still_held(&lease)?;
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
            TokenSource::Provider(layer) => {
                let layer = layer.as_deref().ok_or(WriteError::IdentityUnverified)?;
                let header = async {
                    match &parts.guarded {
                        Some(guarded) => tokio::time::timeout(IDENTITY_TIMEOUT, guarded.header())
                            .await
                            .map_err(|_| WriteError::IdentityUnverified)?
                            .map_err(|_| WriteError::IdentityUnverified),
                        None => provider_header(layer).await,
                    }
                };
                tokio::select! {
                    biased;
                    () = lease.revoked() => return Err(WriteError::IdentityUnverified),
                    header = header => Some(header?),
                }
            }
        };
        still_held(&lease)?;
        let client = match header {
            Some(header) => crate::cluster::sending_credential(client, BoundHeader(header)),
            None => client,
        };
        let identity = if parts.check_identity {
            Some(authenticated_identity(&client).await?)
        } else {
            None
        };
        still_held(&lease)?;
        Ok(Presented {
            client,
            binding: CredentialBinding {
                material: material.finish().ok_or(WriteError::CredentialUnreadable)?,
                identity,
            },
        })
    }
}

fn still_held(lease: &CredentialLease) -> Result<(), WriteError> {
    if lease.holds() {
        Ok(())
    } else {
        Err(WriteError::IdentityUnverified)
    }
}

/// An auth provider's kube layer, when that layer can run a command and a role can forbid it.
///
/// kube runs that command with `std::process` on a blocking thread, behind a lock of its own, so
/// it cannot be killed, and a request waiting on that lock would start another. So requests take
/// their turn here instead, and the grant is asked once each has its turn. The first denial ends
/// the connection's lease for good: a command may still be finishing, its output is dropped, and
/// only a new connection runs the command again.
#[derive(Clone)]
pub(crate) struct GuardedProvider(Arc<GuardedParts>);

struct GuardedParts {
    layer: Arc<AuthLayer>,
    lease: CredentialLease,
    /// Held until kube's call returns, even if the request that made it has gone, so no request
    /// passes the lease check and then waits on kube's lock.
    turn: Arc<tokio::sync::Mutex<()>>,
}

impl GuardedProvider {
    /// A guard for `layer` under `lease`, or `None` if nothing it does needs one: a fixed token
    /// runs nothing, and a credential no role can deny needs no check.
    pub(crate) fn guarding(layer: &Arc<AuthLayer>, lease: &CredentialLease) -> Option<Self> {
        let refreshes = matches!(layer.layer(()), tower::util::Either::Right(_));
        (refreshes && lease.can_be_denied()).then(|| {
            GuardedProvider(Arc::new(GuardedParts {
                layer: layer.clone(),
                lease: lease.clone(),
                turn: Arc::default(),
            }))
        })
    }

    fn spent(&self) -> bool {
        !self.0.lease.holds()
    }

    /// The `Authorization` value from kube's layer, which may run the command.
    pub(crate) async fn header(&self) -> Result<HeaderValue, BoxError> {
        let parts = &self.0;
        let resolving = async {
            let turn = parts.turn.clone().lock_owned().await;
            if !parts.lease.holds() {
                return Err(crate::auth_plugin::not_allowed().into());
            }
            let (layer, lease) = (parts.layer.clone(), parts.lease.clone());
            tokio::spawn(async move {
                let _turn = turn;
                // The last check: nothing is awaited between it and the call into kube.
                if !lease.holds() {
                    return Err(crate::auth_plugin::not_allowed().into());
                }
                layer_header(&layer).await
            })
            .await
            .unwrap_or_else(|stopped| Err(stopped.into()))
        };
        let header = tokio::select! {
            biased;
            () = parts.lease.revoked() => Err(crate::auth_plugin::not_allowed().into()),
            header = resolving => header,
        }?;
        if parts.lease.holds() {
            Ok(header)
        } else {
            Err(crate::auth_plugin::not_allowed().into())
        }
    }
}

impl<B: Send + 'static> tower::filter::AsyncPredicate<Request<B>> for GuardedProvider {
    type Future = BoxFuture<'static, Result<Request<B>, BoxError>>;
    type Request = Request<B>;

    fn check(&mut self, mut request: Request<B>) -> Self::Future {
        let guarded = self.clone();
        Box::pin(async move {
            let value = guarded.header().await?;
            request.headers_mut().insert(AUTHORIZATION, value);
            Ok(request)
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

/// The `Authorization` value `layer` sets now, resolved once. Nothing is sent to get it.
async fn provider_header(layer: &AuthLayer) -> Result<HeaderValue, WriteError> {
    tokio::time::timeout(IDENTITY_TIMEOUT, layer_header(layer))
        .await
        .map_err(|_| WriteError::IdentityUnverified)?
        .map_err(|_| WriteError::IdentityUnverified)
}

/// What kube's auth layer would set on a request. It may run the provider's command.
async fn layer_header(layer: &AuthLayer) -> Result<HeaderValue, BoxError> {
    let read = tower::service_fn(|request: Request<Body>| {
        // Handed back in a response: the auth layer only wraps a service that answers with one.
        let mut answer = http::Response::new(());
        if let Some(header) = request.headers().get(AUTHORIZATION) {
            answer.headers_mut().insert(AUTHORIZATION, header.clone());
        }
        std::future::ready(Ok::<_, BoxError>(answer))
    });
    // Boxed here, so the future of `present` does not name the layer's service types.
    let resolving: BoxFuture<'static, Result<http::Response<()>, BoxError>> = layer
        .layer(read)
        .oneshot(Request::new(Body::empty()))
        .boxed();
    let resolved = resolving.await?;
    let mut value = resolved
        .headers()
        .get(AUTHORIZATION)
        .cloned()
        .ok_or_else(|| BoxError::from("the auth layer set no credential"))?;
    value.set_sensitive(true);
    Ok(value)
}

/// The bearer token `layer` holds now. For a token file just loaded, that is the one kube read,
/// without reading the file again.
pub(crate) async fn client_token(layer: &AuthLayer) -> Option<SecretString> {
    let header = layer_header(layer).await.ok()?;
    let token = header.to_str().ok()?.strip_prefix("Bearer ")?;
    (!token.is_empty()).then(|| SecretString::from(token.to_owned()))
}

/// The `Authorization` a request must go out with. Only [`sending_credential`] sets it.
///
/// [`sending_credential`]: crate::cluster::sending_credential
#[derive(Clone)]
pub(crate) struct BoundHeader(pub(crate) HeaderValue);

/// The step of the client stack that picks a request's credential: one with a [`BoundHeader`]
/// goes to `direct` with that header and resolves nothing, and any other goes through `authed`.
#[derive(Clone)]
pub(crate) struct AuthUnlessBound<A, S> {
    pub(crate) authed: A,
    pub(crate) direct: S,
}

impl<A, S, B> Service<Request<B>> for AuthUnlessBound<A, S>
where
    A: Service<Request<B>, Response = S::Response, Error = BoxError> + Clone + Send + 'static,
    A::Future: Send,
    S: Service<Request<B>, Error = BoxError> + Clone + Send + 'static,
    S::Response: Send,
    S::Future: Send,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = BoxFuture<'static, Result<S::Response, BoxError>>;

    // Each call drives a clone of the service it picks to readiness.
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        match request.extensions().get::<BoundHeader>().cloned() {
            Some(BoundHeader(value)) => {
                request.headers_mut().insert(AUTHORIZATION, value);
                self.direct.clone().oneshot(request).boxed()
            }
            None => self.authed.clone().oneshot(request).boxed(),
        }
    }
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
    let mut extra: BTreeMap<_, _> = user.extra.unwrap_or_default();
    extra.remove(CREDENTIAL_ID);
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
    use std::sync::Mutex;

    use http::Response;
    use kube::client::ConfigExt as _;
    use kube::config::AuthProviderConfig;
    use tower::util::BoxCloneService;

    use super::*;

    type Seen = Arc<Mutex<Vec<String>>>;
    type Transport = BoxCloneService<Request<Body>, Response<Body>, BoxError>;

    /// A client whose API answers a `SelfSubjectReview` with `answer(authorization)`, and keeps
    /// the `Authorization` of every request.
    fn api(answer: impl Fn(&str) -> (u16, String) + Send + Clone + 'static) -> (Client, Seen) {
        api_behind(|transport| transport, answer)
    }

    /// [`api`], with `auth` as the client's own auth over the transport.
    #[allow(clippy::disallowed_methods)] // No cluster and no credentials: a stub transport.
    fn api_behind<A>(
        auth: impl FnOnce(Transport) -> A,
        answer: impl Fn(&str) -> (u16, String) + Send + Clone + 'static,
    ) -> (Client, Seen)
    where
        A: Service<Request<Body>, Response = Response<Body>, Error = BoxError>
            + Clone
            + Send
            + 'static,
        A::Future: Send,
    {
        let seen = Seen::default();
        let kept = seen.clone();
        let direct = BoxCloneService::new(tower::service_fn(move |request: Request<Body>| {
            let authorization = request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            kept.lock().unwrap().push(authorization.clone());
            let (code, body) = answer(&authorization);
            std::future::ready(Ok::<_, BoxError>(
                Response::builder()
                    .status(code)
                    .header("content-type", "application/json")
                    .body(Body::from(body.into_bytes()))
                    .unwrap(),
            ))
        }));
        let authed = auth(direct.clone());
        let service = AuthUnlessBound { authed, direct };
        (Client::new(service, "default"), seen)
    }

    fn review_of(name: &str) -> (u16, String) {
        review_with(serde_json::json!({"username": name, "groups": ["b", "a"]}))
    }

    fn review_with(user: serde_json::Value) -> (u16, String) {
        let body = serde_json::json!({
            "apiVersion": "authentication.k8s.io/v1",
            "kind": "SelfSubjectReview",
            "metadata": {},
            "status": {"userInfo": user},
        });
        (201, body.to_string())
    }

    /// Kube's auth layer for a provider whose command prints a new token at each run, already
    /// expired so that every resolution runs it, and the file that counts the runs.
    fn rotating_provider(dir: &Path) -> (Arc<AuthLayer>, PathBuf) {
        let (script, count) = (dir.join("provider"), dir.join("count"));
        std::fs::write(
            &script,
            format!(
                "n=$(cat {count} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {count}; printf \
                 '{{\"token\":\"token-%s\",\"expiry\":\"2020-01-01T00:00:00Z\"}}' $n\n",
                count = count.display(),
            ),
        )
        .unwrap();
        let settings = [
            ("cmd-path", "/bin/sh".to_owned()),
            ("cmd-args", script.display().to_string()),
            ("token-key", "{.token}".to_owned()),
            ("expiry-key", "{.expiry}".to_owned()),
        ];
        let mut config = kube::Config::new("https://example.invalid".parse().unwrap());
        config.auth_info.auth_provider = Some(AuthProviderConfig {
            name: "gcp".to_owned(),
            config: settings
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
            other: Default::default(),
        });
        (Arc::new(config.auth_layer().unwrap().unwrap()), count)
    }

    fn runs(count: &Path) -> String {
        std::fs::read_to_string(count).unwrap().trim().to_owned()
    }

    async fn send_review(client: Client) -> Result<SelfSubjectReview, Error> {
        Api::all(client)
            .create(&PostParams::default(), &SelfSubjectReview::default())
            .await
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
        WriteCredential::new(Vec::new(), source, None, None)
    }

    fn plugin() -> WriteCredential {
        WriteCredential::new(Vec::new(), CredentialSource::Plugin, None, None)
    }

    fn provider(layer: Option<Arc<AuthLayer>>) -> WriteCredential {
        WriteCredential::new(Vec::new(), CredentialSource::AuthProvider, None, layer)
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

    #[tokio::test]
    async fn a_provider_token_is_resolved_once_for_the_check_and_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let (layer, count) = rotating_provider(dir.path());
        assert_eq!(runs(&count), "1");
        let auth = layer.clone();
        let (client, seen) = api_behind(move |t| auth.layer(t), review_of);

        let presented = provider(Some(layer)).present(client).await.unwrap();
        send_review(presented.client).await.unwrap();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["Bearer token-2", "Bearer token-2"]
        );
        assert_eq!(runs(&count), "2", "resolved once for both requests");
    }

    #[tokio::test]
    async fn a_provider_token_of_another_identity_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (layer, _count) = rotating_provider(dir.path());
        let auth = layer.clone();
        let (client, _) = api_behind(move |t| auth.layer(t), review_of);
        let credential = provider(Some(layer));

        let reviewed = credential.present(client.clone()).await.unwrap().binding;
        let now = credential.present(client).await.unwrap().binding;
        assert_eq!(reviewed.holds(&now), Err(WriteError::CredentialChanged));
    }

    #[tokio::test]
    async fn a_provider_without_a_header_is_refused() {
        let (client, seen) = api(|_| review_of("someone"));
        let refused = provider(None).present(client).await;
        assert_eq!(refused.err(), Some(WriteError::IdentityUnverified));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_a_bound_request_skips_the_client_auth() {
        let failing = |_: Transport| {
            tower::service_fn(|_: Request<Body>| {
                std::future::ready(Err::<Response<Body>, BoxError>("no credential".into()))
            })
        };
        let (client, seen) = api_behind(failing, |_| review_of("someone"));

        assert!(send_review(client.clone()).await.is_err());
        assert!(seen.lock().unwrap().is_empty(), "an unbound request stops");

        let mut value = HeaderValue::from_static("Bearer bound");
        value.set_sensitive(true);
        let bound = crate::cluster::sending_credential(client, BoundHeader(value));
        send_review(bound).await.unwrap();
        assert_eq!(seen.lock().unwrap().as_slice(), ["Bearer bound"]);
    }

    #[tokio::test]
    async fn a_new_credential_id_keeps_the_binding() {
        let user = |id: &str| {
            serde_json::json!({
                "username": "someone", "uid": "1", "groups": ["a"],
                "extra": {CREDENTIAL_ID: [id], "other": ["kept"]},
            })
        };
        let (first, _) = api(move |_| review_with(user("one")));
        let (second, _) = api(move |_| review_with(user("two")));
        let credential = plugin();
        let reviewed = credential.present(first).await.unwrap().binding;
        let rotated = credential.present(second).await.unwrap().binding;
        reviewed.holds(&rotated).unwrap();
    }

    #[tokio::test]
    async fn every_other_identity_field_stays_bound() {
        let base = serde_json::json!({
            "username": "someone", "uid": "1", "groups": ["a"],
            "extra": {CREDENTIAL_ID: ["one"], "other": ["kept"]},
        });
        let credential = plugin();
        let answer = base.clone();
        let (client, _) = api(move |_| review_with(answer.clone()));
        let reviewed = credential.present(client).await.unwrap().binding;
        for (field, value) in [
            ("username", serde_json::json!("another")),
            ("uid", serde_json::json!("2")),
            ("groups", serde_json::json!(["a", "b"])),
            (
                "extra",
                serde_json::json!({CREDENTIAL_ID: ["one"], "other": ["changed"]}),
            ),
            ("extra", serde_json::json!({CREDENTIAL_ID: ["one"]})),
        ] {
            let mut changed = base.clone();
            changed[field] = value;
            let (client, _) = api(move |_| review_with(changed.clone()));
            let now = credential.present(client).await.unwrap().binding;
            assert_eq!(
                reviewed.holds(&now),
                Err(WriteError::CredentialChanged),
                "{field}"
            );
        }
    }

    #[test]
    fn only_files_in_use_stay_bound() {
        let file = |name: &str, role| HeldFile {
            path: PathBuf::from(name),
            held: None,
            role,
        };
        let files = || {
            vec![
                file("ca", FileRole::Authority),
                file("cert", FileRole::ClientIdentity),
                file("key", FileRole::ClientIdentity),
            ]
        };
        let paths = |files: Vec<HeldFile>| files.into_iter().map(|f| f.path).collect::<Vec<_>>();
        assert_eq!(
            paths(files_in_use(files(), true)),
            [PathBuf::from("ca")],
            "a plugin's pair replaces the client files"
        );
        assert_eq!(paths(files_in_use(files(), false)).len(), 3);
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
        let provided = AuthInfo {
            auth_provider: Some(AuthProviderConfig {
                name: "gcp".to_owned(),
                config: Default::default(),
                other: Default::default(),
            }),
            token: Some(SecretString::from("t")),
            ..AuthInfo::default()
        };
        assert_eq!(
            CredentialSource::of(&provided),
            CredentialSource::AuthProvider
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
