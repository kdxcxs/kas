use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const SYSTEM_ADMIN_ROLE: &str = "00000000-0000-0000-0000-000000000001";
pub const SYSTEM_DRIVER_ROLE: &str = "00000000-0000-0000-0000-000000000004";

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
    pub id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub resources: Vec<String>,
    pub verbs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub id: Uuid,
    pub name: String,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceAccount {
    pub id: Uuid,
    pub name: String,
    pub driver_id: Option<Uuid>,
    pub managed_by: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Role {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub rules: Vec<Rule>,
    pub managed_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleBinding {
    pub id: Uuid,
    pub name: String,
    pub role_id: Uuid,
    pub subjects: Vec<Subject>,
    pub managed_by: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub subject: Subject,
    pub rules: Vec<Rule>,
    pub driver_id: Option<Uuid>,
    pub driver_generation: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedCredential {
    pub id: Uuid,
    pub token: String,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUser {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateServiceAccount {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRole {
    pub name: String,
    pub description: String,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRoleBinding {
    pub name: String,
    pub role_id: Uuid,
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

pub fn allows(rules: &[Rule], resource: &str, verb: &str) -> bool {
    rules.iter().any(|rule| {
        rule.resources
            .iter()
            .any(|value| resource_matches(value, resource))
            && (rule.verbs.iter().any(|value| value == "*")
                || rule.verbs.iter().any(|value| value == verb))
    })
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
        }];
        assert!(allows(&rules, "resources", "get"));
        assert!(!allows(&rules, "resources", "create"));
        assert!(!allows(&rules, "runs", "get"));
        assert!(!allows(&[], "resources", "get"));
    }

    #[test]
    fn watch_permissions_can_target_an_exact_manifest() {
        let rules = vec![Rule {
            resources: vec!["resources/conversation".into()],
            verbs: vec!["get".into(), "list".into(), "watch".into()],
        }];

        assert!(allows(&rules, "resources/conversation", "get"));
        assert!(allows(&rules, "resources/conversation", "list"));
        assert!(allows(&rules, "resources/conversation", "watch"));
        assert!(!allows(&rules, "resources/conversation", "create"));
        assert!(!allows(&rules, "resources/task", "watch"));
    }

    #[test]
    fn resource_wildcards_respect_path_boundaries() {
        let bare_resources = vec![Rule {
            resources: vec!["resources".into()],
            verbs: vec!["*".into()],
        }];
        assert!(allows(&bare_resources, "resources", "get"));
        assert!(!allows(&bare_resources, "resources/conversation", "get"));

        let resource_tree = vec![Rule {
            resources: vec!["resources/conversation/*".into()],
            verbs: vec!["watch".into()],
        }];
        assert!(!allows(&resource_tree, "resources/conversation", "watch"));
        assert!(!allows(
            &resource_tree,
            "resources/conversations/message",
            "watch"
        ));
        assert!(!allows(
            &resource_tree,
            "resources/conversation-extra/message",
            "watch"
        ));
        assert!(allows(
            &resource_tree,
            "resources/conversation/message",
            "watch"
        ));

        let global = vec![Rule {
            resources: vec!["*".into()],
            verbs: vec!["*".into()],
        }];
        assert!(allows(&global, "resources/conversation", "watch"));
        assert!(allows(&global, "resources", "update"));
    }

    #[test]
    fn issued_tokens_are_not_stored_verbatim() {
        let token = issue_token();
        assert!(token.starts_with("kas_"));
        assert_ne!(token_hash(&token), token);
        assert_eq!(token_hash(&token), token_hash(&token));
    }
}
