//! Which clusters are kept watched in the background, and what running their sign-in was agreed
//! to (ADR-0012).
//!
//! An entry names a context at its exact server. When the context's credential is an exec plugin,
//! the entry also holds a [`PluginConsent`]: a keyed seal over the whole definition the person
//! confirmed. Any change to its user, command, arguments, environment or cluster entry breaks the
//! seal, and the plugin does not run until it is confirmed again. The seal is one-way and salted,
//! so `settings.toml` holds nothing of the definition.

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
    /// Agreement to run its exec plugin in the background. `None` for a credential that runs
    /// nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_consent: Option<PluginConsent>,
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

/// A salted seal over a context's connection definition.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConsent {
    salt: String,
    seal: String,
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
        let seal = hmac::sign(&key, &definition.canonical);
        Some(PluginConsent {
            salt: hex(&salt),
            seal: hex(seal.as_ref()),
        })
    }

    /// Whether this seal was made over exactly `definition`.
    pub(crate) fn holds(&self, definition: &PluginDefinition) -> bool {
        let (Some(salt), Some(seal)) = (unhex(&self.salt), unhex(&self.seal)) else {
            return false;
        };
        let key = hmac::Key::new(hmac::HMAC_SHA256, &salt);
        hmac::verify(&key, &definition.canonical, &seal).is_ok()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
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
    /// The user's name and entry and the cluster entry, in a stable form. Holds credentials, so
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
            let Ok(mut value) = serde_json::to_value((user, auth, cluster)) else {
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
    /// Its definition changed since it was turned on: nothing runs until it is confirmed again.
    NeedsConfirming,
    /// A kubeconfig file that can't be read could change its definition. It resumes by itself.
    Paused,
}

/// A cluster's toggle, and what turning it on would agree to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepWatchView {
    pub status: KeepWatching,
    /// The command the confirmation must name, when the credential is an exec plugin.
    pub command: Option<String>,
    /// The definition this view describes. A confirmation sends it back, so one made on a
    /// definition that has changed since is refused.
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
        let text = toml::to_string(&consent).expect("toml");
        assert!(!text.contains("aws") && !text.contains("prod"));
        assert!(!format!("{consent:?}").contains(&consent.seal));
        let broken = PluginConsent {
            salt: consent.salt.clone(),
            seal: "zz".to_owned(),
        };
        assert!(!broken.holds(&found));
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
