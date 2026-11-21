//! Short display forms for long kubeconfig names. The full name always stays available in a
//! tooltip; these only decide what fits in a row. Front ends share them, so the sidebar, the
//! command palette, and any CLI name clusters the same way.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::kubeconfig::ContextInfo;

impl ContextInfo {
    /// The name shown in compact lists; see [`short_name`].
    pub fn short_name(&self) -> String {
        short_name(&self.name)
    }

    /// The second line of a compact row; see [`subtitle`].
    pub fn subtitle(&self) -> String {
        subtitle(&self.name, self.server_host())
    }
}

/// Names longer than this (about what fits in a sidebar row) are shortened in the middle, keeping
/// both ends, since contexts in one account or project often share a long prefix and differ only
/// at the end.
const MAX_NAME_CHARS: usize = 22;

/// The parts of an EKS context name: `arn:aws:eks:<region>:<account>:cluster/<name>`.
struct EksArn<'a> {
    region: &'a str,
    account: &'a str,
    name: &'a str,
}

fn parse_eks_arn(context: &str) -> Option<EksArn<'_>> {
    let rest = context
        .strip_prefix("arn:aws:eks:")
        .or_else(|| context.strip_prefix("arn:aws-cn:eks:"))
        .or_else(|| context.strip_prefix("arn:aws-us-gov:eks:"))?;
    let mut parts = rest.splitn(3, ':');
    let region = parts.next()?;
    let account = parts.next()?;
    let name = parts.next()?.strip_prefix("cluster/")?;
    (!name.is_empty()).then_some(EksArn {
        region,
        account,
        name,
    })
}

/// The name shown in the sidebar: the cluster's own name for EKS ARNs (the limit goes to the part
/// that differs), otherwise the context name, shortened in the middle when long.
pub fn short_name(context: &str) -> String {
    match parse_eks_arn(context) {
        Some(arn) => middle_truncate(arn.name, MAX_NAME_CHARS),
        None => middle_truncate(context, MAX_NAME_CHARS),
    }
}

/// How much of a custom name is shown before the real one, so the real one always fits.
const MAX_NAME_BESIDE_REAL: usize = 16;

/// The longest custom cluster name, in characters.
pub const MAX_CUSTOM_NAME_CHARS: usize = 32;

/// What a custom cluster name may hold besides letters and digits.
pub const CUSTOM_NAME_PUNCTUATION: &str = " -_.:/·()#+@";

/// `name` as a custom cluster name: trimmed, at most [`MAX_CUSTOM_NAME_CHARS`], of letters, digits
/// and [`CUSTOM_NAME_PUNCTUATION`] only. An allowlist, so a control, invisible or combining
/// character cannot make it read as something else. `None` when it is not one.
pub fn clean_custom_name(name: &str) -> Option<String> {
    let name = name.trim();
    // Hangul fillers are letters that draw nothing.
    let allowed = |c: char| {
        (c.is_alphanumeric() && !matches!(c, '\u{115f}' | '\u{1160}' | '\u{3164}' | '\u{ffa0}'))
            || CUSTOM_NAME_PUNCTUATION.contains(c)
    };
    (!name.is_empty() && name.chars().count() <= MAX_CUSTOM_NAME_CHARS && name.chars().all(allowed))
        .then(|| name.to_owned())
}

/// The form in which two cluster names count as the same name. Only for comparing, never shown
/// or saved.
///
/// Folds case, treats any run of [`CUSTOM_NAME_PUNCTUATION`] as one separator, folds fullwidth
/// Latin letters and digits, and maps common Cyrillic and Greek look-alikes to Latin. Other
/// non-Latin letters stay as they are, so names in those scripts remain distinct.
///
/// Case is folded before the look-alike map, so each letter has one Latin form in both cases.
/// That form follows the capital where the two cases resemble different Latin letters: Greek
/// nu is `n`, eta is `h` and upsilon is `y`.
pub fn cluster_name_key(name: &str) -> String {
    let mut key = String::with_capacity(name.len());
    let mut in_separator = false;
    for c in name.chars() {
        if CUSTOM_NAME_PUNCTUATION.contains(c) {
            if !in_separator {
                key.push('-');
            }
            in_separator = true;
            continue;
        }
        in_separator = false;
        // One character at a time: `str::to_lowercase` gives a word-final sigma its own form.
        key.extend(without_width(c).to_lowercase().map(latin_look_alike));
    }
    key
}

/// `c` with a fullwidth Latin letter or digit made ASCII, and Turkish dotted capital I made a
/// plain `I`, whose lowercase would otherwise carry a combining dot.
fn without_width(c: char) -> char {
    match c {
        '\u{ff10}'..='\u{ff19}' | '\u{ff21}'..='\u{ff3a}' | '\u{ff41}'..='\u{ff5a}' => {
            char::from_u32(c as u32 - 0xfee0).unwrap_or(c)
        }
        '\u{130}' => 'I',
        c => c,
    }
}

/// The Latin letter a lowercase Cyrillic or Greek letter reads as, else `c`. Written as escapes
/// because the letters would be indistinguishable here.
fn latin_look_alike(c: char) -> char {
    match c {
        '\u{430}' | '\u{3b1}' => 'a',
        '\u{432}' | '\u{3b2}' => 'b',
        '\u{441}' | '\u{3f2}' => 'c',
        '\u{501}' => 'd',
        '\u{435}' | '\u{3b5}' => 'e',
        '\u{43d}' | '\u{4bb}' | '\u{3b7}' => 'h',
        '\u{456}' | '\u{4cf}' | '\u{3b9}' | '\u{131}' => 'i',
        '\u{458}' | '\u{3f3}' => 'j',
        '\u{43a}' | '\u{3ba}' => 'k',
        '\u{43c}' | '\u{3bc}' | '\u{b5}' => 'm',
        '\u{3bd}' => 'n',
        '\u{43e}' | '\u{3bf}' => 'o',
        '\u{440}' | '\u{3c1}' => 'p',
        '\u{51b}' => 'q',
        '\u{455}' | '\u{17f}' => 's',
        '\u{442}' | '\u{3c4}' => 't',
        '\u{51d}' => 'w',
        '\u{445}' | '\u{3c7}' => 'x',
        '\u{443}' | '\u{4af}' | '\u{3c5}' => 'y',
        '\u{3b6}' => 'z',
        // Final sigma is the same letter as sigma.
        '\u{3c2}' => '\u{3c3}',
        c => c,
    }
}

/// What each context is called on screen: its custom name, else [`short_name`]. Only for
/// display: MCP, `FLINT_CLUSTER`, exposure and trust go by the context name.
///
/// A custom name must not pass for another cluster, so one with the same [`cluster_name_key`] as
/// another context's name or short name, or as another custom name, is not used. Saved names
/// that collide stay in the settings file unchanged; each of those clusters shows its real short
/// name instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClusterNames {
    custom: HashMap<String, String>,
}

impl ClusterNames {
    /// The names usable for `contexts`, out of the settings' `names`.
    pub fn new<'a>(
        contexts: impl IntoIterator<Item = &'a str>,
        names: &BTreeMap<String, String>,
    ) -> Self {
        let contexts: Vec<&str> = contexts.into_iter().collect();
        let wanted: Vec<(&str, String)> = contexts
            .iter()
            .filter_map(|&c| Some((c, clean_custom_name(names.get(c)?)?)))
            .collect();
        let reads_like_another = |context: &str, name: &str| {
            let name = cluster_name_key(name);
            contexts.iter().any(|&other| {
                other != context
                    && (cluster_name_key(other) == name
                        || cluster_name_key(&short_name(other)) == name)
            }) || wanted
                .iter()
                .any(|(other, custom)| *other != context && cluster_name_key(custom) == name)
        };
        let custom = wanted
            .iter()
            .filter(|(context, name)| !reads_like_another(context, name))
            .map(|(context, name)| ((*context).to_owned(), name.clone()))
            .collect();
        ClusterNames { custom }
    }

    /// The custom name of `context`, if it has a usable one.
    pub fn custom(&self, context: &str) -> Option<&str> {
        self.custom.get(context).map(String::as_str)
    }

    /// What `context` is called on screen.
    pub fn of(&self, context: &str) -> String {
        match self.custom(context) {
            Some(name) => name.to_owned(),
            None => short_name(context),
        }
    }

    /// [`ClusterNames::of`], followed by the real short name when a custom name is shown: for
    /// anything that protects or unlocks a cluster, where a name must not hide which one it is.
    pub fn with_real(&self, context: &str) -> String {
        match self.custom(context) {
            Some(name) => format!(
                "{} ({})",
                middle_truncate(name, MAX_NAME_BESIDE_REAL),
                short_name(context)
            ),
            None => short_name(context),
        }
    }

    /// The second line of `context`'s compact row: with a custom name, the real name comes
    /// first, so the row always says which cluster it is.
    pub fn subtitle(&self, context: &str, server_host: Option<&str>) -> String {
        let rest = subtitle(context, server_host);
        match self.custom(context) {
            Some(_) => format!("{} · {rest}", short_name(context)),
            None => rest,
        }
    }
}

/// The second line of a sidebar row: region and account for EKS ARNs (the API host is a random
/// string there), otherwise the API server host.
pub fn subtitle(context: &str, server_host: Option<&str>) -> String {
    match parse_eks_arn(context) {
        Some(arn) => format!("{} · {}", arn.region, arn.account),
        None => server_host.unwrap_or("no server").to_owned(),
    }
}

/// `text` cut to at most `max` characters by replacing its middle with "…".
pub fn middle_truncate(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max || max < 3 {
        return text.to_owned();
    }
    let keep = max - 1;
    let head = keep / 2;
    let tail = keep - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(len - tail).collect();
    format!("{start}…{end}")
}

/// A file's name for compact display, falling back to the whole path.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers UP-01
    #[test]
    fn eks_arns_show_the_cluster_name() {
        let arn = "arn:aws:eks:eu-west-1:123456789012:cluster/payments-prod";
        assert_eq!(short_name(arn), "payments-prod");
        assert_eq!(
            subtitle(arn, Some("ABCDEF.gr7.eu-west-1.eks.amazonaws.com")),
            "eu-west-1 · 123456789012"
        );
        // Not a complete ARN: shortened like any other long name.
        assert_eq!(
            short_name("arn:aws:eks:eu-west-1:1:cluster/"),
            "arn:aws:ek…:1:cluster/"
        );
        // Clusters that share a long prefix stay distinguishable.
        assert_ne!(
            short_name("arn:aws:eks:eu-west-1:1:cluster/payments-production-blue"),
            short_name("arn:aws:eks:eu-west-1:1:cluster/payments-production-green")
        );
    }

    fn names(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(c, n)| ((*c).to_owned(), (*n).to_owned()))
            .collect()
    }

    // covers UP-01
    #[test]
    fn a_custom_name_is_shown_with_the_real_cluster_beside_it() {
        let arn = "arn:aws:eks:eu-west-1:123456789012:cluster/example-main";
        let shown = ClusterNames::new([arn, "kind-dev"], &names(&[(arn, "main")]));
        assert_eq!(shown.of(arn), "main");
        assert_eq!(shown.with_real(arn), "main (example-main)");
        assert_eq!(
            shown.subtitle(arn, Some("host")),
            "example-main · eu-west-1 · 123456789012"
        );
        // Without one, nothing changes.
        assert_eq!(shown.of("kind-dev"), "kind-dev");
        assert_eq!(shown.with_real("kind-dev"), "kind-dev");
        assert_eq!(shown.subtitle("kind-dev", Some("host")), "host");
    }

    // covers UP-01: a name must not make one cluster read as another.
    #[test]
    fn a_custom_name_cannot_pass_for_another_cluster() {
        let arn = "arn:aws:eks:eu-west-1:1:cluster/example-prod";
        for name in ["example-prod", "EXAMPLE-PROD", "prod-context", arn] {
            let shown = ClusterNames::new(["dev", "prod-context", arn], &names(&[("dev", name)]));
            assert_eq!(shown.custom("dev"), None, "{name}");
            assert_eq!(shown.of("dev"), "dev");
        }
        // Two contexts with the same custom name: neither gets it.
        let shown = ClusterNames::new(["a", "b"], &names(&[("a", "main"), ("b", "Main")]));
        assert_eq!((shown.of("a").as_str(), shown.of("b").as_str()), ("a", "b"));
        // Its own name is fine, and a context not in kubeconfig reserves nothing.
        let shown = ClusterNames::new(["a"], &names(&[("a", "A"), ("gone", "b")]));
        assert_eq!(shown.of("a"), "A");
    }

    // covers UP-01
    #[test]
    fn the_name_key_treats_separators_as_one() {
        let key = cluster_name_key("prod-eu");
        for same in [
            "prod_eu",
            "PROD EU",
            "prod.eu",
            "prod:/eu",
            "prod -_ eu",
            "prod·eu",
        ] {
            assert_eq!(cluster_name_key(same), key, "{same}");
        }
        assert_ne!(cluster_name_key("prodeu"), key, "a separator still counts");
        assert_ne!(cluster_name_key("prod-e-u"), key);
    }

    // covers UP-01
    #[test]
    fn the_name_key_maps_look_alikes_in_both_cases() {
        // Cyrillic letters, mixed with Latin or alone, lower and upper.
        assert_eq!(cluster_name_key("pr\u{43e}d"), cluster_name_key("prod"));
        assert_eq!(cluster_name_key("PR\u{41e}D"), cluster_name_key("prod"));
        assert_eq!(
            cluster_name_key("\u{440}\u{430}\u{441}\u{435}"),
            cluster_name_key("pace")
        );
        // Greek omicron and capital alpha, kappa, tau.
        assert_eq!(cluster_name_key("pr\u{3bf}d"), cluster_name_key("prod"));
        assert_eq!(
            cluster_name_key("\u{391}\u{39a}\u{3a4}"),
            cluster_name_key("akt")
        );
        // Fullwidth letters and digits, in both cases.
        assert_eq!(
            cluster_name_key("\u{ff50}\u{ff32}\u{ff4f}\u{ff24}\u{ff11}"),
            cluster_name_key("prod1")
        );
        // Case folds inside a script, including a word-final sigma.
        assert_eq!(
            cluster_name_key("\u{3a3}\u{391}\u{3a3}"),
            cluster_name_key("\u{3c3}\u{3b1}\u{3c2}")
        );
        assert_eq!(cluster_name_key("\u{130}D"), cluster_name_key("id"));
    }

    // covers UP-01: only look-alikes fold, so names in other scripts stay usable and distinct.
    #[test]
    fn non_latin_names_stay_allowed_and_distinct() {
        for name in ["продакшн", "δοκιμή", "本番", "프로덕션"] {
            let shown = ClusterNames::new(["a", "b"], &names(&[("a", name)]));
            assert_eq!(shown.custom("a"), Some(name));
        }
        let shown = ClusterNames::new(["a", "b"], &names(&[("a", "север"), ("b", "запад")]));
        assert_eq!(shown.of("a"), "север");
        assert_eq!(shown.of("b"), "запад");
        let shown = ClusterNames::new(["a", "b"], &names(&[("a", "本番"), ("b", "開発")]));
        assert_eq!(shown.of("a"), "本番");
        assert_eq!(shown.of("b"), "開発");
    }

    // covers UP-01: colliding names, as an older settings file may hold them.
    #[test]
    fn colliding_names_fall_back_to_the_real_names() {
        for (one, two) in [
            ("prod-eu", "prod_eu"),
            ("prod", "pr\u{43e}d"),
            ("prod", "\u{ff50}\u{ff52}\u{ff4f}\u{ff44}"),
        ] {
            let shown = ClusterNames::new(["a", "b"], &names(&[("a", one), ("b", two)]));
            assert_eq!((shown.of("a").as_str(), shown.of("b").as_str()), ("a", "b"));
        }
        // Nor may a name pass for another context through a separator or look-alike.
        for name in ["kind_prod", "kind pr\u{43e}d"] {
            let shown = ClusterNames::new(["dev", "kind-prod"], &names(&[("dev", name)]));
            assert_eq!(shown.custom("dev"), None, "{name}");
        }
        // A context's own name, in another form, is fine.
        let shown = ClusterNames::new(["kind-dev"], &names(&[("kind-dev", "KIND_DEV")]));
        assert_eq!(shown.of("kind-dev"), "KIND_DEV");
    }

    // covers UP-01
    #[test]
    fn custom_names_are_short_and_have_no_hidden_characters() {
        assert_eq!(clean_custom_name("  main  ").as_deref(), Some("main"));
        assert_eq!(
            clean_custom_name("Prod EU · blue").as_deref(),
            Some("Prod EU · blue")
        );
        for bad in [
            "",
            "   ",
            "a\nb",
            "a\u{1b}b",
            "a\u{202e}b",
            "a\u{200b}b",
            "a\u{feff}b",
            "pr\u{34f}od",
            "cafe\u{301}",
            "a\u{3164}b",
            "a\u{fe0f}b",
        ] {
            assert_eq!(clean_custom_name(bad), None, "{bad:?}");
        }
        let longest = "x".repeat(MAX_CUSTOM_NAME_CHARS);
        assert!(clean_custom_name(&longest).is_some());
        assert_eq!(clean_custom_name(&format!("{longest}x")), None);
        // A hand-edited name that is not valid is not shown.
        let shown = ClusterNames::new(["a"], &names(&[("a", "b\u{202e}c")]));
        assert_eq!(shown.of("a"), "a");
    }

    // covers UP-01
    #[test]
    fn long_names_keep_both_ends() {
        let gke = "gke_my-very-long-project-name_europe-west1-b_cluster-one";
        let short = short_name(gke);
        assert_eq!(short.chars().count(), MAX_NAME_CHARS);
        assert!(short.starts_with("gke_my-ver"));
        assert!(short.ends_with("cluster-one"));
        assert_eq!(short_name("kind-flint-dev"), "kind-flint-dev");
        assert_eq!(
            subtitle("kind-flint-dev", Some("127.0.0.1:6443")),
            "127.0.0.1:6443"
        );
        assert_eq!(middle_truncate("abcdef", 5), "ab…ef");
        assert_eq!(middle_truncate("é".repeat(10).as_str(), 4), "é…éé");
    }
}
