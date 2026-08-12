use std::fmt;

pub const MAX_PATH_BYTES: usize = 1024;
pub const MAX_PATH_SEGMENT_BYTES: usize = 128;
pub const PACKAGE_ROOT_PREFIX: &str = "/packages";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    NotAbsolute,
    NotRelative,
    Root,
    TrailingSlash,
    EmptySegment,
    InvalidSegment(String),
    TooLong,
    SegmentTooLong,
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAbsolute => formatter.write_str("path must be absolute"),
            Self::NotRelative => formatter.write_str("package path must start with './'"),
            Self::Root => formatter.write_str("path must identify an object"),
            Self::TrailingSlash => formatter.write_str("path must not have a trailing slash"),
            Self::EmptySegment => formatter.write_str("path must not contain empty segments"),
            Self::InvalidSegment(segment) => write!(
                formatter,
                "path segment {segment:?} must contain only lowercase ASCII letters, digits, and internal '-' characters"
            ),
            Self::TooLong => write!(formatter, "path exceeds {MAX_PATH_BYTES} bytes"),
            Self::SegmentTooLong => write!(
                formatter,
                "path segment exceeds {MAX_PATH_SEGMENT_BYTES} bytes"
            ),
        }
    }
}

impl std::error::Error for PathError {}

pub fn validate_path(path: &str) -> Result<(), PathError> {
    validate_absolute(path, false)
}

pub fn validate_path_pattern(pattern: &str) -> Result<(), PathError> {
    validate_absolute(pattern, true)
}

/// Validates package-only relative notation such as `./roles/driver`.
/// Relative paths are resolved before persistence and are never Resource IDs.
pub fn validate_relative_path(path: &str) -> Result<(), PathError> {
    validate_relative(path, false)
}

pub fn validate_relative_path_pattern(path: &str) -> Result<(), PathError> {
    validate_relative(path, true)
}

fn validate_relative(path: &str, allow_wildcards: bool) -> Result<(), PathError> {
    let Some(relative) = path.strip_prefix("./") else {
        return Err(PathError::NotRelative);
    };
    if relative.is_empty() {
        return Err(PathError::Root);
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    if path.ends_with('/') {
        return Err(PathError::TrailingSlash);
    }
    validate_segments(relative, allow_wildcards)
}

fn validate_absolute(path: &str, allow_wildcards: bool) -> Result<(), PathError> {
    if !path.starts_with('/') {
        return Err(PathError::NotAbsolute);
    }
    if path == "/" {
        return Err(PathError::Root);
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    if path.ends_with('/') {
        return Err(PathError::TrailingSlash);
    }
    validate_segments(&path[1..], allow_wildcards)
}

fn validate_segments(path: &str, allow_wildcards: bool) -> Result<(), PathError> {
    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(PathError::EmptySegment);
        }
        if segment.len() > MAX_PATH_SEGMENT_BYTES {
            return Err(PathError::SegmentTooLong);
        }
        if allow_wildcards && matches!(segment, "*" | "**") {
            continue;
        }
        let valid = segment.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'0'..=b'9' => true,
            b'-' => index != 0 && index + 1 != segment.len(),
            _ => false,
        });
        if !valid {
            return Err(PathError::InvalidSegment(segment.into()));
        }
    }
    Ok(())
}

pub fn path_matches(pattern: &str, path: &str) -> bool {
    if validate_path_pattern(pattern).is_err() || validate_path(path).is_err() {
        return false;
    }
    let pattern = split_path(pattern);
    let path = split_path(path);
    let mut memo = vec![vec![None; path.len() + 1]; pattern.len() + 1];
    matches_segments(&pattern, &path, 0, 0, &mut memo)
}

/// Returns the stable Package Root containing `path`.
///
/// Package Roots have exactly two identity segments below `/packages`, for
/// example `/packages/kas/agent`. Descendants remain in the same sandbox.
pub fn package_root_for_path(path: &str) -> Option<String> {
    validate_path(path).ok()?;
    let segments = split_path(path);
    if segments.len() < 3 || segments[0] != "packages" {
        return None;
    }
    Some(format!("/packages/{}/{}", segments[1], segments[2]))
}

/// Returns the Package Root when `path` is the canonical Manifest location
/// `/packages/{publisher}/{package}/manifest`.
pub fn package_root_for_manifest_path(path: &str) -> Option<String> {
    let root = package_root_for_path(path)?;
    (path == format!("{root}/manifest")).then_some(root)
}

pub fn package_manifest_path(package_root: &str) -> Option<String> {
    let root = package_root_for_path(package_root)?;
    (root == package_root).then(|| format!("{root}/manifest"))
}

fn split_path(path: &str) -> Vec<&str> {
    path[1..].split('/').collect()
}

fn matches_segments(
    pattern: &[&str],
    path: &[&str],
    pattern_index: usize,
    path_index: usize,
    memo: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(result) = memo[pattern_index][path_index] {
        return result;
    }
    let result = if pattern_index == pattern.len() {
        path_index == path.len()
    } else if pattern[pattern_index] == "**" {
        matches_segments(pattern, path, pattern_index + 1, path_index, memo)
            || (path_index < path.len()
                && matches_segments(pattern, path, pattern_index, path_index + 1, memo))
    } else {
        path_index < path.len()
            && (pattern[pattern_index] == "*" || pattern[pattern_index] == path[path_index])
            && matches_segments(pattern, path, pattern_index + 1, path_index + 1, memo)
    };
    memo[pattern_index][path_index] = Some(result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concrete_paths_have_one_canonical_spelling() {
        for valid in [
            "/agents/alice",
            "/packages/sha256/0123456789abcdef",
            "/organizations/acme/projects/payment-api",
        ] {
            assert_eq!(validate_path(valid), Ok(()), "{valid}");
        }
        for invalid in [
            "/Agents/alice",
            "/agents/alice_1",
            "/agents/张三",
            "/agents/alice%20smith",
            "/agents/-alice",
            "/agents/alice-",
            "/agents/*",
            "/agents//alice",
            "/agents/alice/",
        ] {
            assert!(validate_path(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn patterns_use_only_complete_wildcard_segments() {
        assert_eq!(validate_path_pattern("/organizations/*/agents/**"), Ok(()));
        assert!(validate_path_pattern("/manifests/integration-*").is_err());
        assert!(path_matches(
            "/organizations/*/agents/**",
            "/organizations/acme/agents/primary/credentials"
        ));
        assert!(!path_matches(
            "/organizations/*/agents/*",
            "/organizations/acme/agents/primary/credentials"
        ));
    }

    #[test]
    fn package_relative_paths_are_not_persistent_paths() {
        assert_eq!(validate_relative_path("./roles/driver"), Ok(()));
        assert!(validate_path("./roles/driver").is_err());
        assert!(validate_relative_path("./roles/Driver").is_err());
        assert!(validate_relative_path("../roles/driver").is_err());
    }

    #[test]
    fn package_roots_and_manifests_have_canonical_locations() {
        assert_eq!(
            package_root_for_path("/packages/kas/agent/resources/main"),
            Some("/packages/kas/agent".into())
        );
        assert_eq!(
            package_root_for_manifest_path("/packages/kas/agent/manifest"),
            Some("/packages/kas/agent".into())
        );
        assert_eq!(
            package_manifest_path("/packages/kas/agent"),
            Some("/packages/kas/agent/manifest".into())
        );
        assert_eq!(
            package_root_for_manifest_path("/packages/kas/agent/manifests/agent"),
            None
        );
        assert_eq!(package_root_for_path("/agents/main"), None);
    }
}
