//! Short display forms for long kubeconfig names. The full name always stays available in a
//! tooltip; these only decide what fits in a row. Front ends share them, so the sidebar, the
//! command palette, and any CLI name clusters the same way.

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

/// The name shown in the sidebar: `cluster/<name>` for EKS ARNs, otherwise the context name,
/// shortened in the middle when long.
pub fn short_name(context: &str) -> String {
    match parse_eks_arn(context) {
        Some(arn) => middle_truncate(&format!("cluster/{}", arn.name), MAX_NAME_CHARS),
        None => middle_truncate(context, MAX_NAME_CHARS),
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
        assert_eq!(short_name(arn), "cluster/payments-prod");
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
