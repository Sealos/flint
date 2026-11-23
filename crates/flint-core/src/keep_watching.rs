//! Which clusters are kept watched in the background, and what running their sign-in was agreed
//! to (ADR-0012).
//!
//! An entry in the settings names a context at its exact server. Agreement to run an exec plugin
//! is never saved: it is a seal held in memory for the running app, over the whole
//! definition the person confirmed. Any change to the context, its user or its cluster entry
//! breaks it, and after a restart the plugin does not run until it is confirmed again.
//!
//! It is a per-cluster toggle, off by default. Turning it on starts from a [`PluginOffer`], which
//! the core makes for the definition the toggle showed. Approving it gives a [`PluginApproval`]
//! the core accepts once.

use kube::config::{AuthInfo, Kubeconfig};
use ring::hmac;
use ring::rand::{SecureRandom as _, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::credential::CredentialSource;

/// The most clusters that can be watched, one feed each: what a trend store holds.
pub const MAX_WATCHED_CLUSTERS: usize = crate::metrics_trend::MAX_CLUSTERS;

/// The refusal when [`MAX_WATCHED_CLUSTERS`] are already watched.
pub fn too_many_watched() -> String {
    format!("Flint can keep watching at most {MAX_WATCHED_CLUSTERS} clusters. Turn one off first.")
}

const SALT_BYTES: usize = 16;

/// A cluster kept watched in the background.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedCluster {
    pub context: String,
    /// The exact API server it was turned on for.
    pub server: String,
}

/// The entries of `watched_clusters` this version can read. One it can't is left out, and never
/// makes the settings file unreadable, which would also hide every cluster from the MCP server.
pub(crate) fn readable_watched<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<WatchedCluster>, D::Error> {
    Ok(watched_entries(toml::Value::deserialize(deserializer)?))
}

/// The readable entries of a `watched_clusters` value.
pub(crate) fn watched_entries(value: toml::Value) -> Vec<WatchedCluster> {
    let toml::Value::Array(entries) = value else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| entry.try_into().ok())
        .collect()
}

/// Agreement to run one exact definition, for as long as the app runs. Neither saved nor shown.
pub(crate) struct PluginConsent {
    salt: [u8; SALT_BYTES],
    seal: Vec<u8>,
}

impl std::fmt::Debug for PluginConsent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginConsent").finish_non_exhaustive()
    }
}

impl PluginConsent {
    /// A seal over `definition`, or `None` if the system has no randomness to give.
    pub(crate) fn seal(definition: &PluginDefinition) -> Option<Self> {
        let mut salt = [0u8; SALT_BYTES];
        SystemRandom::new().fill(&mut salt).ok()?;
        let key = hmac::Key::new(hmac::HMAC_SHA256, &salt);
        let seal = hmac::sign(&key, &definition.canonical).as_ref().to_vec();
        Some(PluginConsent { salt, seal })
    }

    /// Whether this seal was made over exactly `definition`.
    pub(crate) fn holds(&self, definition: &PluginDefinition) -> bool {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.salt);
        hmac::verify(&key, &definition.canonical, &self.seal).is_ok()
    }
}

/// What identifies one offer. Random, so it can't be worked out from anything shown.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OfferTicket([u8; SALT_BYTES]);

impl OfferTicket {
    /// A new ticket, or `None` if the system has no randomness to give.
    pub(crate) fn new() -> Option<Self> {
        let mut ticket = [0u8; SALT_BYTES];
        SystemRandom::new().fill(&mut ticket).ok()?;
        Some(OfferTicket(ticket))
    }
}

/// A key for this run of the app, under which a view names the exact definition it shows.
pub(crate) struct ViewKey(hmac::Key);

impl ViewKey {
    /// A new key, or `None` if the system has no randomness to give.
    pub(crate) fn new() -> Option<Self> {
        let mut key = [0u8; SALT_BYTES];
        SystemRandom::new().fill(&mut key).ok()?;
        Some(ViewKey(hmac::Key::new(hmac::HMAC_SHA256, &key)))
    }

    /// What names `definition` in a view. Says nothing of it, and differs after any change.
    pub(crate) fn name(&self, definition: &PluginDefinition) -> u64 {
        let tag = hmac::sign(&self.0, &definition.canonical);
        let mut name = [0u8; 8];
        name.copy_from_slice(&tag.as_ref()[..8]);
        u64::from_le_bytes(name)
    }
}

/// One turn of a cluster's background sign-in toggle to on, for the definition it showed, from
/// [`CoreHandle::offer_keep_watching`](crate::CoreHandle::offer_keep_watching).
pub struct PluginOffer {
    pub(crate) context: String,
    pub(crate) ticket: OfferTicket,
}

impl std::fmt::Debug for PluginOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginOffer")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

impl PluginOffer {
    /// The person turned the toggle on. Call this only from that toggle's click handler.
    pub fn approve(self) -> PluginApproval {
        PluginApproval {
            context: self.context,
            ticket: self.ticket,
        }
    }
}

/// A confirmed [`PluginOffer`], for [`Command::ApproveKeepWatching`](crate::Command). The core
/// accepts it once, and only while the definition is the one the offer showed.
#[derive(Clone)]
pub struct PluginApproval {
    pub(crate) context: String,
    pub(crate) ticket: OfferTicket,
}

impl std::fmt::Debug for PluginApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginApproval")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

/// How a context signs in, for whether it can be watched in the background.
#[derive(Debug)]
pub(crate) enum SignIn {
    /// Runs nothing: a certificate, a token or a token file.
    Static,
    Plugin(PluginDefinition),
    /// An auth provider's command, which cannot be stopped once it runs: never in the background.
    Provider,
    /// The context or its user can't be resolved.
    Unknown,
}

/// A context's exec plugin definition.
pub(crate) struct PluginDefinition {
    /// What the confirmation names.
    pub(crate) command: String,
    /// The named context and its user and cluster entries, in a stable form. Holds credentials, so
    /// it never leaves this struct except through a seal.
    canonical: Vec<u8>,
}

impl std::fmt::Debug for PluginDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginDefinition")
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

/// How `context` in `kubeconfig` signs in.
pub(crate) fn sign_in_of(kubeconfig: &Kubeconfig, context: &str) -> SignIn {
    let Some(entry) = kubeconfig
        .contexts
        .iter()
        .find(|named| named.name == context)
        .and_then(|named| named.context.as_ref())
    else {
        return SignIn::Unknown;
    };
    let Some(user) = &entry.user else {
        return SignIn::Static;
    };
    let Some(auth) = kubeconfig
        .auth_infos
        .iter()
        .find(|named| &named.name == user)
        .and_then(|named| named.auth_info.as_ref())
    else {
        return SignIn::Unknown;
    };
    match CredentialSource::of(auth) {
        CredentialSource::AuthProvider => SignIn::Provider,
        CredentialSource::Plugin => {
            let cluster = kubeconfig
                .clusters
                .iter()
                .find(|named| named.name == entry.cluster);
            let Ok(mut value) = serde_json::to_value((context, entry, user, auth, cluster)) else {
                return SignIn::Unknown;
            };
            // Exec `env` items are maps, serialized in a different order at each load.
            value.sort_all_objects();
            match shown_command(auth) {
                Some(command) => SignIn::Plugin(PluginDefinition {
                    command,
                    canonical: value.to_string().into_bytes(),
                }),
                None => SignIn::Unknown,
            }
        }
        CredentialSource::Definition | CredentialSource::TokenFile(_) => SignIn::Static,
    }
}

/// The plugin's program as written, and its arguments with anything that looks secret hidden.
fn shown_command(auth: &AuthInfo) -> Option<String> {
    let exec = auth.exec.as_ref()?;
    let program = exec.command.as_deref()?.trim();
    if program.is_empty() {
        return None;
    }
    let mut parts = vec![program.to_owned()];
    let mut hide_next = false;
    for arg in exec.args.iter().flatten() {
        if std::mem::take(&mut hide_next) {
            parts.push("[hidden]".to_owned());
            continue;
        }
        match arg.split_once('=') {
            Some((name, _)) if secret_name(name) => parts.push(format!("{name}=[hidden]")),
            Some(_) => parts.push(arg.clone()),
            None => {
                hide_next = arg.starts_with('-') && secret_name(arg);
                parts.push(arg.clone());
            }
        }
    }
    Some(parts.join(" "))
}

fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "credential", "key"]
        .iter()
        .any(|word| name.contains(word))
}

/// What a cluster's toggle says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepWatching {
    Off,
    Watching,
    /// Stopped after a failure, until the person acts.
    SignInNeeded,
    /// Its sign-in has no agreement for this definition, because it changed or the app restarted:
    /// nothing runs until it is confirmed again.
    NeedsConfirming,
    /// A kubeconfig file that can't be read could change its definition. It resumes by itself.
    Paused,
}

/// A cluster's background sign-in toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundSignIn {
    /// Its credential runs nothing, so there is no toggle.
    NotNeeded,
    /// Shown disabled: an auth provider, or a user Flint can't read.
    Unavailable,
    Off,
    On,
}

/// A cluster's toggles, and what turning them on would agree to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepWatchView {
    pub status: KeepWatching,
    pub sign_in: BackgroundSignIn,
    /// The command the background sign-in toggle names, when the credential is an exec plugin.
    pub command: Option<String>,
    /// Names the exact plugin definition on show. Turning the toggle on sends it back, so a click
    /// made on a definition that has changed since grants nothing.
    pub shown_sign_in: Option<u64>,
    /// The definition this view describes. Turning on a credential that runs nothing sends it
    /// back, so a request made on a definition that has changed since is refused. It is not
    /// consent to run a plugin, which takes a [`PluginApproval`].
    pub definition: Option<u64>,
    /// Why it cannot be turned on, if so.
    pub refusal: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(user: &str) -> String {
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: https://a.example\n\
             contexts:\n- name: dev\n  context:\n    cluster: c\n    user: u\nusers:\n- name: u\n  user:\n{user}"
        )
    }

    fn config(user: &str) -> Kubeconfig {
        Kubeconfig::from_yaml(&text(user)).expect("a kubeconfig")
    }

    const PLUGIN: &str = "    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      command: /usr/bin/aws\n      args: [eks, get-token, --token, abc, --key=xyz]\n      env:\n      - name: AWS_PROFILE\n        value: prod\n";

    fn definition(kubeconfig: &Kubeconfig) -> PluginDefinition {
        match sign_in_of(kubeconfig, "dev") {
            SignIn::Plugin(definition) => definition,
            other => panic!("not a plugin: {other:?}"),
        }
    }

    #[test]
    fn the_command_is_named_with_secret_arguments_hidden() {
        assert_eq!(
            definition(&config(PLUGIN)).command,
            "/usr/bin/aws eks get-token --token [hidden] --key=[hidden]"
        );
    }

    #[test]
    fn consent_is_void_on_every_definition_change() {
        let base = config(PLUGIN);
        let consent = PluginConsent::seal(&definition(&base)).expect("a seal");
        assert!(consent.holds(&definition(&base)));
        let original = text(PLUGIN);
        let changed = |from: &str, to: &str| {
            assert!(original.contains(from), "{from} not in the fixture");
            let edited = Kubeconfig::from_yaml(&original.replace(from, to)).expect("a kubeconfig");
            assert!(!consent.holds(&definition(&edited)), "{from} -> {to}");
        };
        changed("/usr/bin/aws", "/tmp/aws");
        changed("get-token", "get-other");
        changed("AWS_PROFILE", "AWS_OTHER");
        changed("value: prod", "value: staging");
        changed("https://a.example", "https://b.example");
        changed("user: u\n", "user: u\n    namespace: other\n");
        let renamed = original
            .replace("user: u\n", "user: v\n")
            .replace("- name: u\n", "- name: v\n");
        let renamed = Kubeconfig::from_yaml(&renamed).expect("a kubeconfig");
        assert!(!consent.holds(&definition(&renamed)), "another user");
    }

    #[test]
    fn a_seal_shows_nothing_and_a_broken_one_never_holds() {
        let kubeconfig = config(PLUGIN);
        let found = definition(&kubeconfig);
        let consent = PluginConsent::seal(&found).expect("a seal");
        assert_eq!(format!("{consent:?}"), "PluginConsent { .. }");
        let broken = PluginConsent {
            salt: consent.salt,
            seal: vec![0; consent.seal.len()],
        };
        assert!(!broken.holds(&found));
    }

    #[test]
    fn a_saved_consent_field_is_ignored() {
        let entry: WatchedCluster = toml::from_str(
            "context = \"dev\"\nserver = \"https://a.example\"\n\
             [plugin_consent]\nsalt = \"00\"\nseal = \"00\"\n",
        )
        .expect("an entry");
        let saved = toml::to_string(&entry).expect("toml");
        assert!(!saved.contains("plugin_consent"), "{saved}");
    }

    #[test]
    fn sign_in_kinds() {
        assert!(matches!(
            sign_in_of(&config("    token: abc\n"), "dev"),
            SignIn::Static
        ));
        assert!(matches!(
            sign_in_of(
                &config("    auth-provider:\n      name: oidc\n      config: {}\n"),
                "dev"
            ),
            SignIn::Provider
        ));
        assert!(matches!(
            sign_in_of(&config(PLUGIN), "missing"),
            SignIn::Unknown
        ));
    }
}
