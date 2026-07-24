use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Action {
    pub path: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
}

pub type ActionDefinition = Action;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum KindSelector {
    One(ObjectKind),
    Many(Vec<ObjectKind>),
    Any(AnyKind),
}

impl KindSelector {
    pub fn matches(&self, kind: ObjectKind) -> bool {
        match self {
            Self::One(expected) => *expected == kind,
            Self::Many(expected) => expected.contains(&kind),
            Self::Any(_) => true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AnyKind {
    #[serde(rename = "*")]
    Any,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkDirection {
    Source,
    Target,
    Either,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkSelector {
    pub relation_path: String,
    #[serde(default = "default_link_direction")]
    pub direction: LinkDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<Box<ObjectSelector>>,
}

fn default_link_direction() -> LinkDirection {
    LinkDirection::Either
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ObjectSelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<KindSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkSelector>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any_of: Vec<ObjectSelector>,
}

pub const STATE_PENDING: &str = "pending";
pub const STATE_AVAILABLE: &str = "available";
pub const STATE_DELETED: &str = "deleted";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    OneToOne,
    OneToMany,
    ManyToOne,
    ManyToMany,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnSourceDelete {
    Unlink,
    Cascade,
}

fn default_on_source_delete() -> OnSourceDelete {
    OnSourceDelete::Unlink
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelationRole {
    ManifestMember,
    ResourceManifest,
    RunResource,
    RunAction,
    RunDriver,
    DriverServiceAccount,
    RoleBindingRole,
    RoleBindingSubject,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Relation {
    pub path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<RelationRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse_name: Option<String>,
    pub sources: Vec<ObjectSelector>,
    pub targets: Vec<ObjectSelector>,
    #[serde(rename = "type")]
    pub relation_type: RelationType,
    #[serde(default)]
    pub ensure: bool,
    #[serde(default = "default_on_source_delete")]
    pub on_source_delete: OnSourceDelete,
    #[serde(default)]
    pub metadata_schema: Value,
}

pub type RelationDefinition = Relation;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriverRuntime {
    Process,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    Never,
    OnFailure,
    Always,
}

fn default_restart_policy() -> RestartPolicy {
    RestartPolicy::OnFailure
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DriverDefinition {
    pub path: String,
    pub runtime: DriverRuntime,
    pub entrypoint: String,
    pub service_account: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_restart_policy")]
    pub restart: RestartPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RbacRuleDefinition {
    pub resources: Vec<String>,
    pub verbs: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceAccountDefinition {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemRole {
    Admin,
    Editor,
    Viewer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleDefinition {
    pub path: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub rules: Vec<RbacRuleDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_role: Option<SystemRole>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RbacSubjectKind {
    User,
    ServiceAccount,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RbacSubjectDefinition {
    pub kind: RbacSubjectKind,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleBindingDefinition {
    pub path: String,
    pub name: String,
    pub role_path: String,
    pub subjects: Vec<RbacSubjectDefinition>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestRbac {
    #[serde(default)]
    pub service_accounts: Vec<ServiceAccountDefinition>,
    #[serde(default)]
    pub roles: Vec<RoleDefinition>,
    #[serde(default)]
    pub role_bindings: Vec<RoleBindingDefinition>,
}

pub type ManifestRbacDefinition = ManifestRbac;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestDefinition {
    pub path: String,
    pub name: String,
    pub version: u32,
    pub description: String,
    pub resource_schema: Value,
    #[serde(default)]
    pub states: Vec<String>,
    #[serde(default = "default_resource_state")]
    pub default_state: String,
    #[serde(default = "default_resource_state")]
    pub initial_state: String,
    #[serde(default)]
    pub actions: Vec<ActionDefinition>,
    #[serde(default)]
    pub relations: Vec<RelationDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<DriverDefinition>,
    #[serde(default)]
    pub rbac: ManifestRbacDefinition,
}

fn default_resource_state() -> String {
    STATE_AVAILABLE.into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestDefinitionError {
    InvalidManifestPath,
    InvalidMemberPath(String),
    InvalidEntrypoint(String),
}

impl fmt::Display for ManifestDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidManifestPath => formatter.write_str("manifest path must be absolute"),
            Self::InvalidMemberPath(path) => {
                write!(formatter, "invalid manifest member path {path:?}")
            }
            Self::InvalidEntrypoint(path) => {
                write!(formatter, "invalid package entrypoint {path:?}")
            }
        }
    }
}

impl std::error::Error for ManifestDefinitionError {}

impl ManifestDefinition {
    /// Resolves package-local object paths into canonical public object paths.
    ///
    /// The returned value is suitable for persistence. Package files such as the
    /// driver entrypoint intentionally remain relative to the package root.
    pub fn resolve(
        self,
        package_digest: impl Into<String>,
    ) -> Result<CreateManifest, ManifestDefinitionError> {
        if !is_absolute_object_path(&self.path) {
            return Err(ManifestDefinitionError::InvalidManifestPath);
        }
        let manifest_path = self.path.clone();
        let actions = self
            .actions
            .into_iter()
            .map(|mut action| {
                action.path = resolve_member_path(&manifest_path, &action.path)?;
                Ok(action)
            })
            .collect::<Result<Vec<_>, ManifestDefinitionError>>()?;
        let relations = self
            .relations
            .into_iter()
            .map(|mut relation| {
                relation.path = resolve_member_path(&manifest_path, &relation.path)?;
                for selector in relation
                    .sources
                    .iter_mut()
                    .chain(relation.targets.iter_mut())
                {
                    resolve_selector_paths(&manifest_path, selector)?;
                }
                Ok(relation)
            })
            .collect::<Result<Vec<_>, ManifestDefinitionError>>()?;
        let driver = self
            .driver
            .map(|mut driver| {
                driver.path = resolve_member_path(&manifest_path, &driver.path)?;
                validate_package_entrypoint(&driver.entrypoint)?;
                driver.service_account =
                    resolve_reference_path(&manifest_path, &driver.service_account)?;
                Ok(driver)
            })
            .transpose()?;
        let mut rbac = self.rbac;
        for service_account in &mut rbac.service_accounts {
            service_account.path = resolve_member_path(&manifest_path, &service_account.path)?;
        }
        for role in &mut rbac.roles {
            role.path = resolve_member_path(&manifest_path, &role.path)?;
            for rule in &mut role.rules {
                for path in &mut rule.paths {
                    *path = resolve_reference_path(&manifest_path, path)?;
                }
            }
        }
        for binding in &mut rbac.role_bindings {
            binding.path = resolve_member_path(&manifest_path, &binding.path)?;
            binding.role_path = resolve_reference_path(&manifest_path, &binding.role_path)?;
            for subject in &mut binding.subjects {
                subject.path = resolve_reference_path(&manifest_path, &subject.path)?;
            }
        }
        if let Some(driver) = &driver {
            let declared = rbac
                .service_accounts
                .iter()
                .any(|account| account.path == driver.service_account);
            if !declared {
                return Err(ManifestDefinitionError::InvalidMemberPath(
                    driver.service_account.clone(),
                ));
            }
        }

        Ok(CreateManifest {
            path: self.path,
            name: self.name,
            version: self.version,
            description: self.description,
            resource_schema: self.resource_schema,
            states: self.states,
            default_state: self.default_state,
            initial_state: self.initial_state,
            actions,
            relations,
            driver,
            rbac,
            package_digest: package_digest.into(),
        })
    }
}

fn resolve_selector_paths(
    manifest_path: &str,
    selector: &mut ObjectSelector,
) -> Result<(), ManifestDefinitionError> {
    if let Some(path) = selector.path.as_mut() {
        *path = resolve_reference_path(manifest_path, path)?;
    }
    for link in &mut selector.links {
        link.relation_path = resolve_reference_path(manifest_path, &link.relation_path)?;
        if let Some(object) = link.object.as_mut() {
            resolve_selector_paths(manifest_path, object)?;
        }
    }
    for alternative in &mut selector.any_of {
        resolve_selector_paths(manifest_path, alternative)?;
    }
    Ok(())
}

fn resolve_reference_path(
    manifest_path: &str,
    path: &str,
) -> Result<String, ManifestDefinitionError> {
    if path == "." {
        return Ok(manifest_path.to_owned());
    }
    if path.starts_with("./") {
        return resolve_member_path(manifest_path, path);
    }
    if path.starts_with('/') && !path.contains("/../") && !path.ends_with("/..") {
        return Ok(path.to_owned());
    }
    Err(ManifestDefinitionError::InvalidMemberPath(path.to_owned()))
}

fn resolve_member_path(
    manifest_path: &str,
    member_path: &str,
) -> Result<String, ManifestDefinitionError> {
    let Some(relative) = member_path.strip_prefix("./") else {
        return Err(ManifestDefinitionError::InvalidMemberPath(
            member_path.to_owned(),
        ));
    };
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.ends_with('/')
        || relative
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ManifestDefinitionError::InvalidMemberPath(
            member_path.to_owned(),
        ));
    }
    Ok(format!("{manifest_path}/{relative}"))
}

fn validate_package_entrypoint(entrypoint: &str) -> Result<(), ManifestDefinitionError> {
    let Some(relative) = entrypoint.strip_prefix("./") else {
        return Err(ManifestDefinitionError::InvalidEntrypoint(
            entrypoint.to_owned(),
        ));
    };
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.ends_with('/')
        || relative
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ManifestDefinitionError::InvalidEntrypoint(
            entrypoint.to_owned(),
        ));
    }
    Ok(())
}

fn is_absolute_object_path(path: &str) -> bool {
    path.starts_with('/')
        && path != "/"
        && !path.ends_with('/')
        && !path
            .trim_start_matches('/')
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub path: String,
    pub name: String,
    pub version: u32,
    pub description: String,
    pub resource_schema: Value,
    pub states: Vec<String>,
    pub default_state: String,
    pub initial_state: String,
    pub actions: Vec<Action>,
    pub relations: Vec<Relation>,
    pub driver: Option<DriverDefinition>,
    pub rbac: ManifestRbac,
    pub package_digest: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resource {
    pub path: String,
    pub name: String,
    pub spec: Value,
    pub status: Value,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriverState {
    Stopped,
    Starting,
    Ready,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriverDesiredState {
    Stopped,
    Running,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Driver {
    pub path: String,
    pub desired_state: DriverDesiredState,
    pub state: DriverState,
    pub generation: u64,
    pub process_id: Option<u32>,
    pub metadata: Value,
    pub started_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Run {
    pub path: String,
    pub request_id: Uuid,
    pub driver_generation: Option<u64>,
    pub input: Value,
    pub status: RunStatus,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub sequence: u64,
    pub event_type: EventType,
    pub object_kind: ObjectKind,
    pub object_path: String,
    pub revision: Option<u64>,
    pub value: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Created,
    Updated,
    Deleted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Manifest,
    Action,
    Relation,
    Resource,
    Driver,
    Run,
    Link,
    User,
    ServiceAccount,
    Role,
    RoleBinding,
    Credential,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectRef {
    pub kind: ObjectKind,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Link {
    pub path: String,
    pub source: Option<ObjectRef>,
    pub relation_path: String,
    pub target: Option<ObjectRef>,
    pub spec: Value,
    pub status: Value,
    pub metadata: Value,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateManifest {
    pub path: String,
    pub name: String,
    pub version: u32,
    pub description: String,
    pub resource_schema: Value,
    pub states: Vec<String>,
    pub default_state: String,
    pub initial_state: String,
    pub actions: Vec<Action>,
    pub relations: Vec<Relation>,
    pub driver: Option<DriverDefinition>,
    pub rbac: ManifestRbac,
    pub package_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateResource {
    pub path: String,
    pub manifest: String,
    pub name: String,
    pub spec: Value,
    #[serde(default)]
    pub links: Vec<PlannedLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateResource {
    pub expected_revision: u64,
    pub spec: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateLink {
    pub path: String,
    pub source: Option<ObjectRef>,
    pub relation_path: String,
    pub target: Option<ObjectRef>,
    #[serde(default = "available_link_state")]
    pub spec: Value,
    #[serde(default = "available_link_state")]
    pub status: Value,
    #[serde(default)]
    pub metadata: Value,
}

fn available_link_state() -> Value {
    serde_json::json!({ "state": STATE_AVAILABLE })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateLink {
    pub expected_revision: u64,
    pub source: Option<ObjectRef>,
    pub target: Option<ObjectRef>,
    pub status: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkFilter {
    pub source: Option<ObjectRef>,
    pub relation_path: Option<String>,
    pub target: Option<ObjectRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedResource {
    pub path: String,
    pub manifest: String,
    pub name: String,
    pub spec: Value,
    #[serde(default)]
    pub links: Vec<PlannedLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedLink {
    pub path: String,
    pub source: Option<ObjectRef>,
    pub relation_path: String,
    pub target: Option<ObjectRef>,
    #[serde(default = "available_link_state")]
    pub spec: Value,
    #[serde(default = "available_link_state")]
    pub status: Value,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Mutation {
    CreateResource {
        resource: PlannedResource,
    },
    UpdateResource {
        resource_path: String,
        expected_revision: u64,
        spec: Value,
    },
    DeleteResource {
        resource_path: String,
        expected_revision: u64,
    },
    CreateLink {
        link: PlannedLink,
    },
    UpdateLink {
        link_path: String,
        expected_revision: u64,
        source: Option<ObjectRef>,
        target: Option<ObjectRef>,
        status: Value,
    },
    DeleteLink {
        link_path: String,
    },
    CreateServiceAccount {
        path: String,
        name: String,
    },
    DeleteServiceAccount {
        path: String,
    },
    UpdateResourceStatus {
        resource_path: String,
        expected_revision: u64,
        status: Value,
    },
    CompleteRun {
        run_path: String,
        result: RunResult,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DriverExecution {
    pub output: Value,
    #[serde(default)]
    pub mutations: Vec<Mutation>,
}

impl From<Value> for DriverExecution {
    fn from(output: Value) -> Self {
        Self {
            output,
            mutations: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventFilter {
    pub object_kind: Option<ObjectKind>,
    pub object_path: Option<String>,
    pub after_sequence: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateResourceStatus {
    pub driver_path: String,
    pub driver_generation: u64,
    pub expected_revision: u64,
    pub status: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRun {
    pub path: String,
    pub request_id: Uuid,
    pub resource: String,
    pub action: String,
    pub input: Value,
    #[serde(default)]
    pub links: Vec<PlannedLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FinishRun {
    pub driver_generation: u64,
    #[serde(flatten)]
    pub result: RunResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunResult {
    Succeeded { output: Value },
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReconcileObject {
    Resource(Resource),
    Link(Link),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriverWork {
    Reconcile {
        object: ReconcileObject,
    },
    Run {
        run: Box<Run>,
        resource: Resource,
        action: Action,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending,
    Acked,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DriverDelivery {
    pub id: Uuid,
    pub driver_path: String,
    pub generation: u64,
    pub work: DriverWork,
    pub status: DeliveryStatus,
    pub created_at: DateTime<Utc>,
    pub acked_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverReady {
    pub generation: u64,
    pub process_id: u32,
    #[serde(default)]
    pub metadata: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_manifest_package_members_and_selector_references() {
        let definition = ManifestDefinition {
            path: "/manifests/agent".into(),
            name: "agent".into(),
            version: 1,
            description: String::new(),
            resource_schema: json!({"type": "object"}),
            states: vec![],
            default_state: STATE_AVAILABLE.into(),
            initial_state: STATE_PENDING.into(),
            actions: vec![Action {
                path: "./actions/message".into(),
                name: "message".into(),
                description: String::new(),
                input_schema: json!({}),
                output_schema: json!({}),
            }],
            relations: vec![Relation {
                path: "./relations/instance".into(),
                name: "instance".into(),
                role: Some(RelationRole::ResourceManifest),
                inverse_name: None,
                sources: vec![ObjectSelector {
                    kind: Some(KindSelector::One(ObjectKind::Resource)),
                    ..ObjectSelector::default()
                }],
                targets: vec![ObjectSelector {
                    kind: Some(KindSelector::One(ObjectKind::Manifest)),
                    path: Some(".".into()),
                    ..ObjectSelector::default()
                }],
                relation_type: RelationType::ManyToOne,
                ensure: false,
                on_source_delete: OnSourceDelete::Unlink,
                metadata_schema: json!({}),
            }],
            driver: Some(DriverDefinition {
                path: "./driver".into(),
                runtime: DriverRuntime::Process,
                entrypoint: "./driver/bin/agent".into(),
                service_account: "./service-accounts/driver".into(),
                args: vec![],
                restart: RestartPolicy::OnFailure,
            }),
            rbac: ManifestRbac {
                service_accounts: vec![ServiceAccountDefinition {
                    path: "./service-accounts/driver".into(),
                    name: "driver".into(),
                }],
                roles: vec![RoleDefinition {
                    path: "./roles/driver".into(),
                    name: "driver".into(),
                    description: String::new(),
                    rules: vec![RbacRuleDefinition {
                        resources: vec!["resources".into()],
                        verbs: vec!["get".into()],
                        paths: vec!["./resources/**".into()],
                    }],
                    system_role: None,
                }],
                role_bindings: vec![RoleBindingDefinition {
                    path: "./role-bindings/driver".into(),
                    name: "driver".into(),
                    role_path: "./roles/driver".into(),
                    subjects: vec![RbacSubjectDefinition {
                        kind: RbacSubjectKind::ServiceAccount,
                        path: "./service-accounts/driver".into(),
                    }],
                }],
            },
        };

        let installed = definition.resolve("sha256").unwrap();
        assert_eq!(
            installed.actions[0].path,
            "/manifests/agent/actions/message"
        );
        assert_eq!(
            installed.relations[0].path,
            "/manifests/agent/relations/instance"
        );
        assert_eq!(
            installed.relations[0].targets[0].path.as_deref(),
            Some("/manifests/agent")
        );
        assert_eq!(
            installed.driver.as_ref().unwrap().path,
            "/manifests/agent/driver"
        );
        assert_eq!(
            installed.driver.as_ref().unwrap().entrypoint,
            "./driver/bin/agent"
        );
        assert_eq!(
            installed.driver.as_ref().unwrap().service_account,
            "/manifests/agent/service-accounts/driver"
        );
        assert_eq!(
            installed.rbac.roles[0].path,
            "/manifests/agent/roles/driver"
        );
        assert_eq!(
            installed.rbac.roles[0].rules[0].paths[0],
            "/manifests/agent/resources/**"
        );
        assert_eq!(
            installed.rbac.role_bindings[0].role_path,
            "/manifests/agent/roles/driver"
        );
        assert_eq!(
            installed.rbac.role_bindings[0].subjects[0].path,
            "/manifests/agent/service-accounts/driver"
        );
    }

    #[test]
    fn kind_selector_supports_one_many_and_wildcard_json() {
        let one: KindSelector = serde_json::from_value(json!("user")).unwrap();
        let many: KindSelector = serde_json::from_value(json!(["user", "resource"])).unwrap();
        let any: KindSelector = serde_json::from_value(json!("*")).unwrap();

        assert!(one.matches(ObjectKind::User));
        assert!(!one.matches(ObjectKind::Resource));
        assert!(many.matches(ObjectKind::User));
        assert!(many.matches(ObjectKind::Resource));
        assert!(any.matches(ObjectKind::Relation));
    }

    #[test]
    fn rejects_entrypoints_outside_the_package() {
        let mut definition = ManifestDefinition {
            path: "/manifests/agent".into(),
            name: "agent".into(),
            version: 1,
            description: String::new(),
            resource_schema: json!({}),
            states: vec![],
            default_state: STATE_AVAILABLE.into(),
            initial_state: STATE_PENDING.into(),
            actions: vec![],
            relations: vec![],
            driver: Some(DriverDefinition {
                path: "./driver".into(),
                runtime: DriverRuntime::Process,
                entrypoint: "../agent".into(),
                service_account: "./service-accounts/driver".into(),
                args: vec![],
                restart: RestartPolicy::Never,
            }),
            rbac: ManifestRbac {
                service_accounts: vec![ServiceAccountDefinition {
                    path: "./service-accounts/driver".into(),
                    name: "driver".into(),
                }],
                roles: vec![],
                role_bindings: vec![],
            },
        };
        assert!(matches!(
            definition.clone().resolve("digest"),
            Err(ManifestDefinitionError::InvalidEntrypoint(_))
        ));
        definition.driver.as_mut().unwrap().entrypoint = "/tmp/agent".into();
        assert!(matches!(
            definition.resolve("digest"),
            Err(ManifestDefinitionError::InvalidEntrypoint(_))
        ));
    }

    #[test]
    fn builtin_manifests_parse_and_resolve() {
        let core: ManifestDefinition =
            serde_json::from_str(include_str!("../../../builtins/core/manifest.json")).unwrap();
        let auth: ManifestDefinition =
            serde_json::from_str(include_str!("../../../builtins/auth/manifest.json")).unwrap();

        let core = core.resolve("core-digest").unwrap();
        let auth = auth.resolve("auth-digest").unwrap();

        assert_eq!(core.name, "core");
        assert!(core
            .relations
            .iter()
            .any(|relation| relation.role == Some(RelationRole::ResourceManifest)));
        assert_eq!(auth.name, "auth");
        assert!(auth
            .rbac
            .roles
            .iter()
            .any(|role| role.system_role == Some(SystemRole::Admin)));
    }
}
