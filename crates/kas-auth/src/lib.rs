use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::fmt;
use uuid::Uuid;

pub mod resources {
    pub const MANIFESTS: &str = "manifests";
    pub const ACTIONS: &str = "actions";
    pub const RELATIONS: &str = "relations";
    pub const RESOURCES: &str = "resources";
    pub const DRIVERS: &str = "drivers";
    pub const RUNS: &str = "runs";
    pub const LINKS: &str = "links";
}

pub mod verbs {
    pub const GET: &str = "get";
    pub const LIST: &str = "list";
    pub const CREATE: &str = "create";
    pub const UPDATE: &str = "update";
    pub const PATCH: &str = "patch";
    pub const DELETE: &str = "delete";
    pub const LINK: &str = "link";
    pub const INVOKE: &str = "invoke";
    pub const USE: &str = "use";
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    User,
    ServiceAccount,
}

impl SubjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ServiceAccount => "service_account",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subject {
    pub kind: SubjectKind,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub resources: Vec<String>,
    pub verbs: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub path: String,
    pub name: String,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceAccount {
    pub path: String,
    pub name: String,
    pub driver_path: Option<String>,
    pub managed_by: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Role {
    pub path: String,
    pub name: String,
    pub description: String,
    pub rules: Vec<Rule>,
    pub managed_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleBinding {
    pub path: String,
    pub name: String,
    pub role_path: String,
    pub subjects: Vec<Subject>,
    pub managed_by: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub subject: Subject,
    pub rules: Vec<Rule>,
    pub driver_path: Option<String>,
    pub driver_generation: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedCredential {
    pub path: String,
    pub token: String,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUser {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateServiceAccount {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRole {
    pub path: String,
    pub name: String,
    pub description: String,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRoleBinding {
    pub path: String,
    pub name: String,
    pub role_path: String,
    pub subjects: Vec<Subject>,
}

pub fn issue_token() -> String {
    format!("kas_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    NotAbsolute,
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
            Self::Root => formatter.write_str("path must identify an object"),
            Self::TrailingSlash => formatter.write_str("path must not have a trailing slash"),
            Self::EmptySegment => formatter.write_str("path must not contain empty segments"),
            Self::InvalidSegment(segment) => {
                write!(formatter, "path contains invalid segment {segment:?}")
            }
            Self::TooLong => formatter.write_str("path exceeds 1024 bytes"),
            Self::SegmentTooLong => formatter.write_str("path segment exceeds 255 bytes"),
        }
    }
}

impl std::error::Error for PathError {}

pub fn validate_path(path: &str) -> Result<(), PathError> {
    validate_path_inner(path, false)
}

pub fn validate_path_pattern(pattern: &str) -> Result<(), PathError> {
    validate_path_inner(pattern, true)
}

fn validate_path_inner(path: &str, allow_wildcards: bool) -> Result<(), PathError> {
    if !path.starts_with('/') {
        return Err(PathError::NotAbsolute);
    }
    if path == "/" {
        return Err(PathError::Root);
    }
    if path.len() > 1024 {
        return Err(PathError::TooLong);
    }
    if path.ends_with('/') {
        return Err(PathError::TrailingSlash);
    }

    for segment in path[1..].split('/') {
        if segment.is_empty() {
            return Err(PathError::EmptySegment);
        }
        if segment.len() > 255 {
            return Err(PathError::SegmentTooLong);
        }
        if segment == "." || segment == ".." {
            return Err(PathError::InvalidSegment(segment.into()));
        }
        if segment
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
            || (!allow_wildcards && (segment == "*" || segment == "**"))
            || (segment.contains('*') && segment != "*" && segment != "**")
        {
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

pub fn path_pattern_contains(container: &str, candidate: &str) -> bool {
    if validate_path_pattern(container).is_err() || validate_path_pattern(candidate).is_err() {
        return false;
    }
    let container = split_path(container);
    let candidate = split_path(candidate);
    let mut alphabet = container
        .iter()
        .chain(candidate.iter())
        .filter(|segment| **segment != "*" && **segment != "**")
        .copied()
        .collect::<Vec<_>>();
    alphabet.push("\0other");
    alphabet.sort_unstable();
    alphabet.dedup();

    let candidate_start = epsilon_closure(&candidate, &[0]);
    let container_start = epsilon_closure(&container, &[0]);
    let mut pending = VecDeque::from([(candidate_start, container_start, false)]);
    let mut visited = HashSet::new();

    while let Some((candidate_states, container_states, consumed)) = pending.pop_front() {
        if !visited.insert((candidate_states.clone(), container_states.clone(), consumed)) {
            continue;
        }
        if consumed
            && candidate_states.contains(&candidate.len())
            && !container_states.contains(&container.len())
        {
            return false;
        }
        for symbol in &alphabet {
            let next_candidate = transition(&candidate, &candidate_states, symbol);
            if next_candidate.is_empty() {
                continue;
            }
            let next_container = transition(&container, &container_states, symbol);
            pending.push_back((next_candidate, next_container, true));
        }
    }
    true
}

fn split_path(path: &str) -> Vec<&str> {
    path[1..].split('/').collect()
}

fn epsilon_closure(pattern: &[&str], states: &[usize]) -> Vec<usize> {
    let mut result = states.to_vec();
    let mut index = 0;
    while index < result.len() {
        let state = result[index];
        if state < pattern.len() && pattern[state] == "**" && !result.contains(&(state + 1)) {
            result.push(state + 1);
        }
        index += 1;
    }
    result.sort_unstable();
    result
}

fn transition(pattern: &[&str], states: &[usize], symbol: &str) -> Vec<usize> {
    let mut next = Vec::new();
    for state in states {
        if *state == pattern.len() {
            continue;
        }
        match pattern[*state] {
            "**" => next.push(*state),
            "*" => next.push(*state + 1),
            literal if literal == symbol => next.push(*state + 1),
            _ => {}
        }
    }
    next.sort_unstable();
    next.dedup();
    epsilon_closure(pattern, &next)
}

pub fn allows(rules: &[Rule], resource: &str, verb: &str, path: Option<&str>) -> bool {
    rules.iter().any(|rule| {
        rule.resources
            .iter()
            .any(|value| resource_matches(value, resource))
            && (rule.verbs.iter().any(|value| value == "*")
                || rule.verbs.iter().any(|value| value == verb))
            && match path {
                Some(path) => {
                    rule.paths.is_empty()
                        || rule.paths.iter().any(|pattern| path_matches(pattern, path))
                }
                None => rule.paths.is_empty(),
            }
    })
}

/// Checks permission to invoke a concrete Action object.
pub fn allows_action_invoke(rules: &[Rule], action_path: &str) -> bool {
    allows(rules, resources::ACTIONS, verbs::INVOKE, Some(action_path))
}

/// Checks permission to create a Link using a concrete Relation object.
///
/// Endpoint `link` permissions are intentionally separate and must also be
/// checked by the caller.
pub fn allows_relation_use(rules: &[Rule], relation_path: &str) -> bool {
    allows(rules, resources::RELATIONS, verbs::USE, Some(relation_path))
}

pub fn rules_are_subset(proposed: &[Rule], caller: &[Rule]) -> bool {
    proposed.iter().all(|proposed_rule| {
        if proposed_rule
            .paths
            .iter()
            .any(|path| validate_path_pattern(path).is_err())
        {
            return false;
        }
        proposed_rule.resources.iter().all(|resource| {
            proposed_rule.verbs.iter().all(|verb| {
                if proposed_rule.paths.is_empty() {
                    caller.iter().any(|caller_rule| {
                        caller_rule.paths.is_empty()
                            && rule_covers_resource_and_verb(caller_rule, resource, verb)
                    })
                } else {
                    proposed_rule.paths.iter().all(|path| {
                        caller.iter().any(|caller_rule| {
                            rule_covers_resource_and_verb(caller_rule, resource, verb)
                                && (caller_rule.paths.is_empty()
                                    || caller_rule
                                        .paths
                                        .iter()
                                        .any(|owned| path_pattern_contains(owned, path)))
                        })
                    })
                }
            })
        })
    })
}

fn rule_covers_resource_and_verb(rule: &Rule, resource: &str, verb: &str) -> bool {
    rule.resources
        .iter()
        .any(|owned| resource_pattern_contains(owned, resource))
        && rule.verbs.iter().any(|owned| owned == "*" || owned == verb)
}

fn resource_pattern_contains(container: &str, candidate: &str) -> bool {
    if container == "*" || container == candidate {
        return true;
    }
    if candidate == "*" {
        return false;
    }
    let Some(container_prefix) = container.strip_suffix("/*") else {
        return false;
    };
    let Some(candidate_prefix) = candidate.strip_suffix("/*") else {
        return resource_matches(container, candidate);
    };
    candidate_prefix == container_prefix
        || candidate_prefix
            .strip_prefix(container_prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn resource_matches(pattern: &str, resource: &str) -> bool {
    if pattern == "*" || pattern == resource {
        return true;
    }

    // A trailing wildcard grants access only to descendants on a path-segment
    // boundary. For example, `resources/chat/*` matches
    // `resources/chat/messages`, while `resources/chat` and
    // `resources/chatter/messages` remain distinct resources.
    pattern
        .strip_suffix("/*")
        .filter(|prefix| !prefix.is_empty())
        .is_some_and(|prefix| {
            resource
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('/') && suffix.len() > 1)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_are_additive_and_default_to_deny() {
        let rules = vec![Rule {
            resources: vec!["resources".into()],
            verbs: vec!["get".into(), "list".into()],
            paths: vec![],
        }];
        assert!(allows(&rules, "resources", "get", None));
        assert!(allows(&rules, "resources", "get", Some("/resources/a")));
        assert!(!allows(&rules, "resources", "create", None));
        assert!(!allows(&rules, "runs", "get", None));
        assert!(!allows(&[], "resources", "get", None));
    }

    #[test]
    fn path_scoped_permissions_can_target_an_exact_manifest() {
        let rules = vec![Rule {
            resources: vec!["resources/conversation".into()],
            verbs: vec!["get".into(), "list".into()],
            paths: vec!["/conversations/team-a/**".into()],
        }];

        assert!(allows(
            &rules,
            "resources/conversation",
            "get",
            Some("/conversations/team-a/one")
        ));
        assert!(!allows(
            &rules,
            "resources/conversation",
            "get",
            Some("/conversations/team-ab/one")
        ));
        assert!(!allows(&rules, "resources/conversation", "list", None));
        assert!(!allows(
            &rules,
            "resources/conversation",
            "create",
            Some("/conversations/team-a/two")
        ));
        assert!(!allows(
            &rules,
            "resources/task",
            "get",
            Some("/conversations/team-a/one")
        ));
    }

    #[test]
    fn resource_wildcards_respect_path_boundaries() {
        let bare_resources = vec![Rule {
            resources: vec!["resources".into()],
            verbs: vec!["*".into()],
            paths: vec![],
        }];
        assert!(allows(&bare_resources, "resources", "get", None));
        assert!(!allows(
            &bare_resources,
            "resources/conversation",
            "get",
            None
        ));

        let resource_tree = vec![Rule {
            resources: vec!["resources/conversation/*".into()],
            verbs: vec!["get".into()],
            paths: vec![],
        }];
        assert!(!allows(
            &resource_tree,
            "resources/conversation",
            "get",
            None
        ));
        assert!(!allows(
            &resource_tree,
            "resources/conversations/message",
            "get",
            None
        ));
        assert!(!allows(
            &resource_tree,
            "resources/conversation-extra/message",
            "get",
            None
        ));
        assert!(allows(
            &resource_tree,
            "resources/conversation/message",
            "get",
            None
        ));

        let global = vec![Rule {
            resources: vec!["*".into()],
            verbs: vec!["*".into()],
            paths: vec![],
        }];
        assert!(allows(&global, "resources/conversation", "get", None));
        assert!(allows(&global, "resources", "update", None));
    }

    #[test]
    fn validates_canonical_absolute_paths() {
        assert_eq!(validate_path("/computers/a"), Ok(()));
        assert!(matches!(
            validate_path("computers/a"),
            Err(PathError::NotAbsolute)
        ));
        assert!(matches!(validate_path("/"), Err(PathError::Root)));
        assert!(matches!(
            validate_path("/computers//a"),
            Err(PathError::EmptySegment)
        ));
        assert!(matches!(
            validate_path("/computers/../a"),
            Err(PathError::InvalidSegment(_))
        ));
        assert!(validate_path("/computers/*").is_err());
        assert_eq!(validate_path_pattern("/computers/**"), Ok(()));
    }

    #[test]
    fn segment_globs_match_without_prefix_leaks() {
        assert!(path_matches("/computers/*", "/computers/a"));
        assert!(!path_matches("/computers/*", "/computers/a/child"));
        assert!(path_matches("/computers/**", "/computers/a/child"));
        assert!(path_matches("/computers/**", "/computers"));
        assert!(!path_matches("/computers/a/**", "/computers/ab/child"));
        assert!(path_matches(
            "/teams/**/computers/*",
            "/teams/a/zone/one/computers/c1"
        ));
    }

    #[test]
    fn path_pattern_containment_handles_exact_star_and_double_star() {
        assert!(path_pattern_contains(
            "/computers/team-a/**",
            "/computers/team-a/rack-1/**"
        ));
        assert!(path_pattern_contains("/computers/**", "/computers/*"));
        assert!(!path_pattern_contains(
            "/computers/team-a/**",
            "/computers/**"
        ));
        assert!(!path_pattern_contains(
            "/computers/*",
            "/computers/team-a/**"
        ));
    }

    #[test]
    fn proposed_rules_must_be_covered_without_cross_rule_composition() {
        let caller = vec![
            Rule {
                resources: vec!["resources/computer".into()],
                verbs: vec!["get".into(), "patch".into()],
                paths: vec!["/computers/team-a/**".into()],
            },
            Rule {
                resources: vec!["resources/computer".into()],
                verbs: vec!["delete".into()],
                paths: vec!["/computers/team-b/**".into()],
            },
        ];
        let allowed = vec![Rule {
            resources: vec!["resources/computer".into()],
            verbs: vec!["get".into()],
            paths: vec!["/computers/team-a/rack-1/**".into()],
        }];
        assert!(rules_are_subset(&allowed, &caller));

        let path_escalation = vec![Rule {
            resources: vec!["resources/computer".into()],
            verbs: vec!["get".into()],
            paths: vec!["/computers/**".into()],
        }];
        assert!(!rules_are_subset(&path_escalation, &caller));

        let combined_escalation = vec![Rule {
            resources: vec!["resources/computer".into()],
            verbs: vec!["delete".into()],
            paths: vec!["/computers/team-a/**".into()],
        }];
        assert!(!rules_are_subset(&combined_escalation, &caller));
    }

    #[test]
    fn unrestricted_paths_can_only_be_delegated_by_unrestricted_rules() {
        let scoped = vec![Rule {
            resources: vec!["resources/computer".into()],
            verbs: vec!["get".into()],
            paths: vec!["/computers/**".into()],
        }];
        let unrestricted = vec![Rule {
            resources: vec!["resources/computer".into()],
            verbs: vec!["get".into()],
            paths: vec![],
        }];
        assert!(!rules_are_subset(&unrestricted, &scoped));
        assert!(rules_are_subset(&scoped, &unrestricted));
        assert!(rules_are_subset(&unrestricted, &unrestricted));

        let invalid = vec![Rule {
            resources: vec!["resources/computer".into()],
            verbs: vec!["get".into()],
            paths: vec!["computers/**".into()],
        }];
        assert!(!rules_are_subset(&invalid, &unrestricted));
    }

    #[test]
    fn issued_tokens_are_not_stored_verbatim() {
        let token = issue_token();
        assert!(token.starts_with("kas_"));
        assert_ne!(token_hash(&token), token);
        assert_eq!(token_hash(&token), token_hash(&token));
    }

    #[test]
    fn action_invocation_and_relation_use_are_path_scoped() {
        let rules = vec![
            Rule {
                resources: vec![resources::ACTIONS.into()],
                verbs: vec![verbs::INVOKE.into()],
                paths: vec!["/manifests/agent/actions/*".into()],
            },
            Rule {
                resources: vec![resources::RELATIONS.into()],
                verbs: vec![verbs::USE.into()],
                paths: vec!["/manifests/agent/relations/**".into()],
            },
        ];

        assert!(allows_action_invoke(
            &rules,
            "/manifests/agent/actions/message"
        ));
        assert!(!allows_action_invoke(
            &rules,
            "/manifests/message/actions/send"
        ));
        assert!(allows_relation_use(
            &rules,
            "/manifests/agent/relations/has-thread"
        ));
        assert!(!allows_relation_use(
            &rules,
            "/relations/system/resource-manifest"
        ));
    }
}
