//! Validation for values interpolated into Kubernetes API paths by Flint or kube.

use kube::api::ApiResource;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PathError {
    #[error("the object's name is not valid")]
    Name,
    #[error("the namespace name is not valid")]
    Namespace,
    #[error("the API resource plural is not valid")]
    Resource,
    #[error("the API group or version is not valid")]
    GroupVersion,
}

/// Lowercase letters, digits and `-`, starting and ending with a letter or digit.
fn is_dns1123_part(part: &str) -> bool {
    let bytes = part.as_bytes();
    let alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    bytes.first().is_some_and(alnum)
        && bytes.last().is_some_and(alnum)
        && bytes.iter().all(|b| alnum(b) || *b == b'-')
}

/// A DNS-1123 label: at most 63 lowercase letters, digits and `-`, starting and ending with a
/// letter or digit.
pub fn is_dns1123_label(name: &str) -> bool {
    name.len() <= 63 && is_dns1123_part(name)
}

/// A DNS-1123 subdomain as Kubernetes checks it: at most 253 characters, parts as in a label
/// joined by `.`, with no limit on the length of one part.
pub fn is_dns1123_subdomain(name: &str) -> bool {
    name.len() <= 253 && name.split('.').all(is_dns1123_part)
}

/// Whether `name` can be sent as one segment of a request path without changing its routing.
pub fn is_path_segment(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && !name
            .chars()
            .any(|c| matches!(c, '/' | '%' | '?' | '#') || c.is_whitespace() || c.is_control())
}

pub(crate) fn validate_names(namespace: Option<&str>, name: Option<&str>) -> Result<(), PathError> {
    if namespace.is_some_and(|ns| !is_dns1123_label(ns)) {
        return Err(PathError::Namespace);
    }
    if name.is_some_and(|name| name.len() > 253 || !is_path_segment(name)) {
        return Err(PathError::Name);
    }
    Ok(())
}

pub(crate) fn validate_group_version(group: &str, version: &str) -> Result<(), PathError> {
    if (group.is_empty() || is_dns1123_subdomain(group)) && is_dns1123_label(version) {
        Ok(())
    } else {
        Err(PathError::GroupVersion)
    }
}

pub(crate) fn validate_resource_plural(plural: &str) -> Result<(), PathError> {
    if is_dns1123_label(plural) {
        Ok(())
    } else {
        Err(PathError::Resource)
    }
}

pub(crate) fn validate_api_version(
    group: &str,
    version: &str,
    api_version: &str,
) -> Result<(), PathError> {
    validate_group_version(group, version)?;
    // kube interpolates api_version verbatim, separately from its check of whether group is empty.
    let matches = if group.is_empty() {
        api_version == version
    } else {
        api_version.split_once('/') == Some((group, version))
    };
    if matches {
        Ok(())
    } else {
        Err(PathError::GroupVersion)
    }
}

pub(crate) fn validate_resource_path(
    resource: &ApiResource,
    namespace: Option<&str>,
    name: Option<&str>,
) -> Result<(), PathError> {
    validate_names(namespace, name)?;
    validate_api_version(&resource.group, &resource.version, &resource.api_version)?;
    validate_resource_plural(&resource.plural)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_object_names_as_path_segments() {
        assert_eq!(validate_names(Some("team-1"), Some("web-1.prod")), Ok(()));
        assert_eq!(validate_names(None, None), Ok(()));
        assert_eq!(validate_names(None, Some(&"x".repeat(253))), Ok(()));
        for name in [
            "system:controller:job-controller",
            "system:node-proxier",
            "A_B",
            "a..b",
            "-a",
            "a-",
        ] {
            assert_eq!(validate_names(None, Some(name)), Ok(()), "{name}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "x?y",
            "x#y",
            "%2F",
            "a b",
            "a\u{00a0}b",
            "a\nb",
            "a\0b",
        ] {
            assert_eq!(
                validate_names(None, Some(bad)),
                Err(PathError::Name),
                "{bad:?}"
            );
        }
        assert_eq!(
            validate_names(None, Some(&"x".repeat(254))),
            Err(PathError::Name)
        );
    }

    #[test]
    fn validates_namespaces_as_dns1123_labels() {
        assert_eq!(validate_names(Some(&"x".repeat(63)), None), Ok(()));
        for bad in ["", "a.b", "A", "-a", "a-", "a/b", "a:b", &"x".repeat(64)] {
            assert_eq!(validate_names(Some(bad), None), Err(PathError::Namespace));
        }
    }

    #[test]
    fn validates_the_segments_kube_uses_for_dynamic_resources() {
        let mut resource = ApiResource {
            group: "example.com".into(),
            version: "v1beta1".into(),
            api_version: "example.com/v1beta1".into(),
            kind: "Widget".into(),
            plural: "widgets".into(),
        };
        assert_eq!(validate_resource_path(&resource, None, None), Ok(()));
        for bad in [
            "",
            "Widgets",
            "widgets.example.com",
            "pods/log",
            "widgets%2f",
        ] {
            resource.plural = bad.into();
            assert_eq!(
                validate_resource_path(&resource, None, None),
                Err(PathError::Resource)
            );
        }
        resource.plural = "widgets".into();
        resource.api_version = "example.com/v1beta1/../v1".into();
        assert_eq!(
            validate_resource_path(&resource, None, None),
            Err(PathError::GroupVersion)
        );
        assert_eq!(validate_api_version("", "v1", "v1"), Ok(()));
        assert_eq!(
            validate_api_version("", "v1/..", "v1/.."),
            Err(PathError::GroupVersion)
        );
    }
}
