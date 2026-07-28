use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::{Deref, DerefMut};
use uuid::Uuid;

pub const STATE_PENDING: &str = "pending";
pub const STATE_AVAILABLE: &str = "available";
pub const STATE_DELETED: &str = "deleted";
pub const MANIFEST_MANIFEST_PATH: &str = "/builtin/manifest";
pub const MANIFEST_PACKAGE_MEDIA_TYPE: &str = "application/vnd.kas.manifest+tar";
pub const BUILTIN_PACKAGE_MEDIA_TYPE: &str = "application/vnd.kas.builtin+json";

/// The only public persistent object in KAS.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resource {
    pub path: String,
    pub metadata: ResourceMetadata,
    pub spec: Value,
    pub status: ResourceStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriverObservation {
    pub driver_revision: u64,
    pub resource_revision: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResourceMetadata {
    #[serde(default)]
    pub manifest: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default, rename = "[kas]")]
    pub kas: KasMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KasMetadata {
    #[serde(default)]
    pub revision: u64,
    /// Runtime generation for singleton Driver Resources. Zero for other
    /// Resource types.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub generation: u64,
    #[serde(default)]
    pub observed: BTreeMap<String, DriverObservation>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub protected: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub managed_by: String,
    /// Content-addressed Package that defines this Resource's Manifest.
    ///
    /// The desired document points at the active Package. The status document
    /// retains the Package used by the last successful owner reconciliation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub package: String,
    #[serde(default)]
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub updated_at: DateTime<Utc>,
}

impl Deref for ResourceMetadata {
    type Target = KasMetadata;

    fn deref(&self) -> &Self::Target {
        &self.kas
    }
}

impl DerefMut for ResourceMetadata {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.kas
    }
}

impl Deref for Resource {
    type Target = ResourceMetadata;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

impl DerefMut for Resource {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.metadata
    }
}

impl Resource {
    pub fn status_metadata(&self, state: impl Into<String>) -> ResourceMetadata {
        let mut metadata = self.metadata.clone();
        metadata.state = state.into();
        metadata
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlannedResourceMetadata {
    pub manifest: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlannedResource {
    pub path: String,
    pub metadata: PlannedResourceMetadata,
    #[serde(default = "default_document")]
    pub spec: Value,
    #[serde(default)]
    pub status: ResourceStatus,
}

impl Deref for PlannedResource {
    type Target = PlannedResourceMetadata;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

impl DerefMut for PlannedResource {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.metadata
    }
}

pub type CreateResource = PlannedResource;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateResourceMetadata {
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpdateResource {
    pub expected_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<UpdateResourceMetadata>,
    pub spec: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpdateResourceStatus {
    pub expected_revision: u64,
    pub status: ResourceStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResourceStatus {
    #[serde(default)]
    pub metadata: ResourceMetadata,
    #[serde(default = "default_document")]
    pub spec: Value,
}

impl Default for ResourceStatus {
    fn default() -> Self {
        Self {
            metadata: ResourceStatusMetadata::default(),
            spec: default_document(),
        }
    }
}

pub type ResourceStatusMetadata = ResourceMetadata;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Created,
    Updated,
    Deleted,
}

/// Events are operational records about Resources, not another object type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub sequence: u64,
    pub event_type: EventType,
    pub resource_path: String,
    pub revision: Option<u64>,
    pub value: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventFilter {
    pub resource_path: Option<String>,
    pub after_sequence: Option<u64>,
    pub limit: Option<usize>,
}

/// A manifest-path predicate used by Relation and watch selectors.
///
/// A JSON string selects one manifest, an array is an OR, and `"*"` selects
/// Resources of every manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ManifestSelector {
    One(String),
    Many(Vec<String>),
}

impl ManifestSelector {
    pub fn matches(&self, manifest: &str) -> bool {
        match self {
            Self::One(expected) => resource_path_matches(expected, manifest),
            Self::Many(expected) => expected
                .iter()
                .any(|value| resource_path_matches(value, manifest)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceSelector {
    pub manifest: ManifestSelector,
    #[serde(default)]
    pub paths: Vec<String>,
}

impl ResourceSelector {
    pub fn matches(&self, resource: &Resource) -> bool {
        self.manifest.matches(&resource.manifest)
            && (self.paths.is_empty()
                || self
                    .paths
                    .iter()
                    .any(|pattern| resource_path_matches(pattern, &resource.path)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestSpec {
    pub version: u32,
    #[serde(default)]
    pub description: String,
    pub resource_schema: Value,
    /// Manifest-specific states. Platform states (`pending`, `available`, and
    /// `deleted`) are always available and must not be repeated here.
    #[serde(default)]
    pub states: Vec<String>,
    pub default_state: String,
    pub initial_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageSpec {
    pub digest: String,
    pub size_bytes: u64,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub manifest: String,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub manifest_version: u32,
}

pub fn package_path_for_digest(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("/packages/sha256/{}", hex.to_ascii_lowercase()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionSpec {
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
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
    ManifestResource,
    PackageManifest,
    ResourceManifest,
    RunResource,
    RunAction,
    RunDriver,
    DriverServiceAccount,
    DriverCredential,
    RoleBinding,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelationSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<RelationRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse_name: Option<String>,
    pub sources: Vec<ResourceSelector>,
    pub targets: Vec<ResourceSelector>,
    #[serde(default = "default_on_source_delete")]
    pub on_source_delete: OnSourceDelete,
    #[serde(default)]
    pub metadata_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkSpec {
    pub relation: String,
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriverWatch {
    pub manifest: ManifestSelector,
    #[serde(default)]
    pub paths: Vec<String>,
}

impl DriverWatch {
    pub fn matches(&self, resource: &Resource) -> bool {
        if !self.manifest.matches(&resource.manifest)
            || (!self.paths.is_empty()
                && !self
                    .paths
                    .iter()
                    .any(|pattern| resource_path_matches(pattern, &resource.path)))
        {
            return false;
        }
        true
    }
}

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
pub struct DriverSpec {
    pub runtime: DriverRuntime,
    pub entrypoint: String,
    pub service_account: String,
    /// Explicit Manifest paths whose Resource status this singleton Driver owns.
    #[serde(default)]
    pub manages: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub watches: Vec<DriverWatch>,
    #[serde(default = "default_restart_policy")]
    pub restart: RestartPolicy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriverState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriverControlState {
    Stopped,
    Running,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunSpec {
    pub request_id: Uuid,
    pub resource: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RbacRuleSpec {
    pub manifests: Vec<String>,
    pub verbs: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemRole {
    Admin,
    Editor,
    Viewer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserSpec {
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceAccountSpec {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleSpec {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub rules: Vec<RbacRuleSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_role: Option<SystemRole>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CredentialSpec {
    pub subject: String,
    /// SHA-256 hash of the bearer token. The plaintext is returned only when
    /// the Credential is issued.
    pub token_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

/// A Resource declaration loaded from one JSON file below `resources/`.
/// Only `path` and Resource references are resolved by the package expander;
/// type-specific content remains in `spec` and `status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResourceDefinition {
    pub path: String,
    pub metadata: PlannedResourceMetadata,
    #[serde(default = "default_document")]
    pub spec: Value,
    #[serde(default)]
    pub status: ResourceStatus,
}

fn default_document() -> Value {
    Value::Object(serde_json::Map::new())
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Deref for ResourceDefinition {
    type Target = PlannedResourceMetadata;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

impl DerefMut for ResourceDefinition {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.metadata
    }
}

/// The transport document stored as `manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ManifestDefinition {
    pub path: String,
    /// Manifest of the emitted Manifest Resource. The self-describing root uses
    /// `"."`, meaning its own path.
    pub manifest: String,
    pub name: String,
    pub version: u32,
    #[serde(default)]
    pub description: String,
    pub resource_schema: Value,
    /// Manifest-specific states only; platform states are implicit.
    #[serde(default)]
    pub states: Vec<String>,
    #[serde(default = "default_resource_state")]
    pub default_state: String,
    #[serde(default = "default_resource_state")]
    pub initial_state: String,
}

fn default_resource_state() -> String {
    STATE_AVAILABLE.into()
}

/// Parsed contents of one KAS package.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageDefinition {
    pub manifest: ManifestDefinition,
    #[serde(default)]
    pub resources: Vec<ResourceDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageExpansion {
    /// Digest of the uploaded artifact. Store installation turns this
    /// transport metadata into a Package Resource.
    pub artifact_digest: String,
    pub resources: Vec<PlannedResource>,
    /// Owning Manifest path for each expanded Resource. A Manifest Resource
    /// owns itself; packaged Resources point to the root Manifest that declared
    /// them.
    #[serde(default)]
    pub resource_owners: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestDefinitionError {
    InvalidManifestPath,
    InvalidResourcePath(String),
    InvalidResourceSpec(String),
    DuplicateResourcePath(String),
    NestedManifest(String),
    MultipleDrivers,
    InvalidEntrypoint(String),
}

impl fmt::Display for ManifestDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidManifestPath => formatter.write_str("manifest path must be absolute"),
            Self::InvalidResourcePath(path) => {
                write!(formatter, "invalid package Resource path {path:?}")
            }
            Self::InvalidResourceSpec(error) => {
                write!(formatter, "invalid package Resource spec: {error}")
            }
            Self::DuplicateResourcePath(path) => {
                write!(formatter, "duplicate package Resource path {path:?}")
            }
            Self::NestedManifest(path) => {
                write!(
                    formatter,
                    "package Resource {path:?} cannot define a nested Manifest"
                )
            }
            Self::MultipleDrivers => {
                formatter.write_str("a Manifest package can declare at most one Driver Resource")
            }
            Self::InvalidEntrypoint(path) => {
                write!(formatter, "invalid package entrypoint {path:?}")
            }
        }
    }
}

impl std::error::Error for ManifestDefinitionError {}

impl PackageDefinition {
    pub fn expand(
        self,
        artifact_digest: impl Into<String>,
    ) -> Result<PackageExpansion, ManifestDefinitionError> {
        if !is_absolute_object_path(&self.manifest.path) {
            return Err(ManifestDefinitionError::InvalidManifestPath);
        }
        let digest = artifact_digest.into();
        let manifest_type = if self.manifest.manifest == "." {
            self.manifest.path.clone()
        } else {
            resolve_reference_path(&self.manifest.path, &self.manifest.manifest)?
        };
        // This is the state of the Manifest Resource itself. The
        // default/initial states declared inside its spec apply to instances
        // defined by this Manifest, not to the Manifest Resource.
        let manifest_status = ResourceStatus::default();
        let manifest_spec = serde_json::to_value(ManifestSpec {
            version: self.manifest.version,
            description: self.manifest.description,
            resource_schema: self.manifest.resource_schema,
            states: self.manifest.states,
            default_state: self.manifest.default_state,
            initial_state: self.manifest.initial_state,
        })
        .expect("ManifestSpec serialization cannot fail");

        let root_path = self.manifest.path;
        let mut resources = Vec::with_capacity(self.resources.len() + 1);
        let mut resource_owners = BTreeMap::new();
        let mut resource_paths = std::collections::BTreeSet::from([root_path.clone()]);
        let mut driver_count = 0;
        resource_owners.insert(root_path.clone(), root_path.clone());
        resources.push(PlannedResource {
            path: root_path.clone(),
            metadata: PlannedResourceMetadata {
                manifest: manifest_type,
                name: self.manifest.name,
                state: String::new(),
            },
            spec: manifest_spec,
            status: manifest_status,
        });
        for mut resource in self.resources {
            resource.path = resolve_reference_path(&root_path, &resource.path)?;
            resource.manifest = resolve_reference_path(&root_path, &resource.manifest)?;
            if resource.manifest == MANIFEST_MANIFEST_PATH {
                return Err(ManifestDefinitionError::NestedManifest(resource.path));
            }
            if !resource_paths.insert(resource.path.clone()) {
                return Err(ManifestDefinitionError::DuplicateResourcePath(
                    resource.path,
                ));
            }
            if resource.manifest == "/builtin/driver" {
                driver_count += 1;
                if driver_count > 1 {
                    return Err(ManifestDefinitionError::MultipleDrivers);
                }
            }
            let resource_manifest = resource.metadata.manifest.clone();
            resolve_embedded_resource_references(
                &root_path,
                &resource_manifest,
                &mut resource.spec,
            )?;
            resource_owners.insert(resource.path.clone(), root_path.clone());
            resources.push(PlannedResource {
                path: resource.path,
                metadata: resource.metadata,
                spec: resource.spec,
                status: resource.status,
            });
        }
        Ok(PackageExpansion {
            artifact_digest: digest,
            resources,
            resource_owners,
        })
    }
}

fn resolve_embedded_resource_references(
    manifest_path: &str,
    resource_manifest: &str,
    spec: &mut Value,
) -> Result<(), ManifestDefinitionError> {
    if let Some(entrypoint) = spec.get("entrypoint").and_then(Value::as_str) {
        validate_package_entrypoint(entrypoint)?;
    }

    match resource_manifest {
        "/builtin/driver" => {
            let mut value: DriverSpec = decode_resource_spec(spec)?;
            value.service_account = resolve_reference_path(manifest_path, &value.service_account)?;
            if value.manages.is_empty() {
                value.manages.push(manifest_path.to_owned());
            } else {
                for managed_manifest in &mut value.manages {
                    *managed_manifest = resolve_reference_path(manifest_path, managed_manifest)?;
                }
            }
            for watch in &mut value.watches {
                resolve_manifest_selector(manifest_path, &mut watch.manifest)?;
                for path in &mut watch.paths {
                    resolve_relative_reference(manifest_path, path)?;
                }
            }
            *spec = serde_json::to_value(value).expect("DriverSpec serialization cannot fail");
        }
        "/builtin/relation" => {
            let mut value: RelationSpec = decode_resource_spec(spec)?;
            for selector in value.sources.iter_mut().chain(&mut value.targets) {
                resolve_manifest_selector(manifest_path, &mut selector.manifest)?;
                for path in &mut selector.paths {
                    resolve_relative_reference(manifest_path, path)?;
                }
            }
            *spec = serde_json::to_value(value).expect("RelationSpec serialization cannot fail");
        }
        "/builtin/link" => {
            let mut value: LinkSpec = decode_resource_spec(spec)?;
            value.relation = resolve_reference_path(manifest_path, &value.relation)?;
            value.source = resolve_reference_path(manifest_path, &value.source)?;
            value.target = resolve_reference_path(manifest_path, &value.target)?;
            *spec = serde_json::to_value(value).expect("LinkSpec serialization cannot fail");
        }
        "/builtin/run" => {
            let mut value: RunSpec = decode_resource_spec(spec)?;
            value.resource = resolve_reference_path(manifest_path, &value.resource)?;
            value.action = resolve_reference_path(manifest_path, &value.action)?;
            resolve_optional_reference(manifest_path, &mut value.driver)?;
            *spec = serde_json::to_value(value).expect("RunSpec serialization cannot fail");
        }
        "/builtin/role" => {
            let mut value: RoleSpec = decode_resource_spec(spec)?;
            for rule in &mut value.rules {
                for selected_manifest in &mut rule.manifests {
                    resolve_relative_reference(manifest_path, selected_manifest)?;
                }
                for path in &mut rule.paths {
                    resolve_relative_reference(manifest_path, path)?;
                }
            }
            *spec = serde_json::to_value(value).expect("RoleSpec serialization cannot fail");
        }
        "/builtin/credential" => {
            let mut value: CredentialSpec = decode_resource_spec(spec)?;
            value.subject = resolve_reference_path(manifest_path, &value.subject)?;
            *spec = serde_json::to_value(value).expect("CredentialSpec serialization cannot fail");
        }
        _ => {}
    }
    Ok(())
}

fn decode_resource_spec<T: for<'de> Deserialize<'de>>(
    spec: &Value,
) -> Result<T, ManifestDefinitionError> {
    serde_json::from_value(spec.clone())
        .map_err(|error| ManifestDefinitionError::InvalidResourceSpec(error.to_string()))
}

fn resolve_manifest_selector(
    manifest_path: &str,
    selector: &mut ManifestSelector,
) -> Result<(), ManifestDefinitionError> {
    match selector {
        ManifestSelector::One(value) => resolve_relative_reference(manifest_path, value),
        ManifestSelector::Many(values) => {
            for value in values {
                resolve_relative_reference(manifest_path, value)?;
            }
            Ok(())
        }
    }
}

fn resolve_optional_reference(
    manifest_path: &str,
    reference: &mut Option<String>,
) -> Result<(), ManifestDefinitionError> {
    if let Some(value) = reference {
        *value = resolve_reference_path(manifest_path, value)?;
    }
    Ok(())
}

fn resolve_relative_reference(
    manifest_path: &str,
    reference: &mut String,
) -> Result<(), ManifestDefinitionError> {
    if reference == "." || reference.starts_with("./") {
        *reference = resolve_reference_path(manifest_path, reference)?;
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
        return resolve_package_resource_path(manifest_path, path);
    }
    if is_absolute_object_path(path) {
        return Ok(path.to_owned());
    }
    Err(ManifestDefinitionError::InvalidResourcePath(
        path.to_owned(),
    ))
}

fn resolve_package_resource_path(
    manifest_path: &str,
    resource_path: &str,
) -> Result<String, ManifestDefinitionError> {
    let Some(relative) = resource_path.strip_prefix("./") else {
        return Err(ManifestDefinitionError::InvalidResourcePath(
            resource_path.to_owned(),
        ));
    };
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.ends_with('/')
        || relative
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ManifestDefinitionError::InvalidResourcePath(
            resource_path.to_owned(),
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

pub fn resource_path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" || pattern == path {
        return true;
    }
    let pattern = pattern.trim_matches('/').split('/').collect::<Vec<_>>();
    let path = path.trim_matches('/').split('/').collect::<Vec<_>>();
    glob_segments_match(&pattern, &path)
}

fn glob_segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((segment, rest)) if *segment == "**" => {
            glob_segments_match(rest, path)
                || (!path.is_empty() && glob_segments_match(pattern, &path[1..]))
        }
        Some((segment, rest)) => {
            !path.is_empty()
                && glob_segment_matches(segment, path[0])
                && glob_segments_match(rest, &path[1..])
        }
    }
}

fn glob_segment_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern == value {
        return true;
    }
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return false;
    }
    let mut remainder = value;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            let Some(next) = remainder.strip_prefix(part) else {
                return false;
            };
            remainder = next;
        } else if index + 1 == parts.len() {
            return remainder.ends_with(part);
        } else {
            let Some(offset) = remainder.find(part) else {
                return false;
            };
            remainder = &remainder[offset + part.len()..];
        }
    }
    parts.last().is_some_and(|part| part.is_empty()) || remainder.is_empty()
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<UpdateResourceMetadata>,
        spec: Value,
    },
    DeleteResource {
        resource_path: String,
        expected_revision: u64,
    },
    UpdateResourceStatus {
        resource_path: String,
        expected_revision: u64,
        status: ResourceStatus,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateRun {
    pub path: String,
    pub request_id: Uuid,
    pub resource: String,
    pub action: String,
    pub input: Value,
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
pub enum DriverWork {
    Reconcile {
        driver_revision: u64,
        resource: Resource,
    },
    Run {
        run: Resource,
        resource: Resource,
        action: Resource,
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
    pub lease_expires_at: DateTime<Utc>,
    pub acked_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    fn resource_is_the_single_persistent_shape() {
        let now = Utc::now();
        let metadata = ResourceMetadata {
            manifest: "/manifests/agent".into(),
            name: "main".into(),
            state: STATE_AVAILABLE.into(),
            kas: KasMetadata {
                revision: 1,
                generation: 0,
                observed: BTreeMap::new(),
                protected: false,
                managed_by: "user".into(),
                package: "/packages/sha256/example".into(),
                created_at: now,
                updated_at: now,
            },
        };
        let resource = Resource {
            path: "/agents/main".into(),
            metadata: metadata.clone(),
            spec: json!({"working_directory": "/workspace"}),
            status: ResourceStatus {
                metadata,
                spec: json!({"working_directory": "/workspace"}),
            },
        };
        assert_eq!(resource.manifest, "/manifests/agent");
        assert_eq!(resource.metadata.kas.observed.len(), 0);
    }

    #[test]
    fn manifest_selector_supports_single_many_and_any() {
        let single: ManifestSelector = serde_json::from_value(json!("/manifests/agent")).unwrap();
        let many: ManifestSelector =
            serde_json::from_value(json!(["/manifests/user", "/manifests/agent"])).unwrap();
        let any: ManifestSelector = serde_json::from_value(json!("*")).unwrap();
        assert!(single.matches("/manifests/agent"));
        assert!(many.matches("/manifests/agent"));
        assert!(any.matches("/manifests/anything"));
    }

    #[test]
    fn artifact_digest_maps_to_a_stable_package_path() {
        let digest = format!("sha256:{}", "A0".repeat(32));
        assert_eq!(
            package_path_for_digest(&digest),
            Some(format!("/packages/sha256/{}", "a0".repeat(32)))
        );
        assert_eq!(package_path_for_digest("sha512:a0b1"), None);
        assert_eq!(package_path_for_digest("sha256:not-hex"), None);
    }

    #[test]
    fn expands_manifest_and_resource_files() {
        let definition = PackageDefinition {
            manifest: ManifestDefinition {
                path: "/manifests/example".into(),
                manifest: MANIFEST_MANIFEST_PATH.into(),
                name: "example".into(),
                version: 1,
                description: "Example".into(),
                resource_schema: json!({"type": "object"}),
                states: vec![],
                default_state: STATE_AVAILABLE.into(),
                initial_state: STATE_PENDING.into(),
            },
            resources: vec![ResourceDefinition {
                path: "./actions/example".into(),
                metadata: PlannedResourceMetadata {
                    manifest: "/builtin/action".into(),
                    name: "example".into(),
                    state: String::new(),
                },
                spec: json!({"description":"","input_schema":{},"output_schema":{}}),
                status: ResourceStatus::default(),
            }],
        };
        let expansion = definition.expand("digest").unwrap();
        assert_eq!(expansion.resources.len(), 2);
        assert_eq!(expansion.resources[0].manifest, MANIFEST_MANIFEST_PATH);
        assert_eq!(
            expansion.resources[1].path,
            "/manifests/example/actions/example"
        );
    }

    #[test]
    fn rejects_manifest_definitions_in_resources() {
        let definition = PackageDefinition {
            manifest: ManifestDefinition {
                path: "/manifests/container".into(),
                manifest: MANIFEST_MANIFEST_PATH.into(),
                name: "container".into(),
                version: 1,
                description: String::new(),
                resource_schema: json!({}),
                states: vec![],
                default_state: STATE_AVAILABLE.into(),
                initial_state: STATE_AVAILABLE.into(),
            },
            resources: vec![ResourceDefinition {
                path: "./nested".into(),
                metadata: PlannedResourceMetadata {
                    manifest: MANIFEST_MANIFEST_PATH.into(),
                    name: "nested".into(),
                    state: String::new(),
                },
                spec: json!({}),
                status: ResourceStatus::default(),
            }],
        };
        assert!(matches!(
            definition.expand("digest"),
            Err(ManifestDefinitionError::NestedManifest(path))
                if path == "/manifests/container/nested"
        ));
    }

    #[test]
    fn rejects_entrypoint_outside_package() {
        let definition = PackageDefinition {
            manifest: ManifestDefinition {
                path: "/manifests/agent".into(),
                manifest: "/builtin/manifest".into(),
                name: "agent".into(),
                version: 1,
                description: String::new(),
                resource_schema: json!({}),
                states: vec![],
                default_state: STATE_AVAILABLE.into(),
                initial_state: STATE_AVAILABLE.into(),
            },
            resources: vec![ResourceDefinition {
                path: "./driver".into(),
                metadata: PlannedResourceMetadata {
                    manifest: "/builtin/driver".into(),
                    name: "driver".into(),
                    state: String::new(),
                },
                spec: json!({
                    "runtime":"process",
                    "entrypoint":"../driver",
                    "service_account":"./service-accounts/driver"
                }),
                status: ResourceStatus::default(),
            }],
        };
        assert!(matches!(
            definition.expand("digest"),
            Err(ManifestDefinitionError::InvalidEntrypoint(_))
        ));
    }

    #[test]
    fn builtin_manifests_are_independent_definitions() {
        let documents = [
            include_str!("../../../builtins/manifest/manifest.json"),
            include_str!("../../../builtins/action/manifest.json"),
            include_str!("../../../builtins/relation/manifest.json"),
            include_str!("../../../builtins/link/manifest.json"),
            include_str!("../../../builtins/driver/manifest.json"),
            include_str!("../../../builtins/run/manifest.json"),
            include_str!("../../../builtins/user/manifest.json"),
            include_str!("../../../builtins/service-account/manifest.json"),
            include_str!("../../../builtins/role/manifest.json"),
            include_str!("../../../builtins/credential/manifest.json"),
            include_str!("../../../builtins/package/manifest.json"),
        ];
        let mut paths = Vec::new();
        for document in documents {
            let definition: ManifestDefinition = serde_json::from_str(document).unwrap();
            assert!(definition.states.iter().all(|state| !matches!(
                state.as_str(),
                STATE_PENDING | STATE_AVAILABLE | STATE_DELETED
            )));
            paths.push(definition.path);
        }

        assert_eq!(paths.len(), 11);
        assert!(paths.contains(&"/builtin/manifest".into()));
        assert!(paths.contains(&"/builtin/package".into()));
    }

    #[test]
    fn old_members_field_is_rejected() {
        let value = json!({
            "path": "/manifests/old",
            "manifest": "/builtin/manifest",
            "name": "old",
            "version": 1,
            "resource_schema": {},
            "default_state": "available",
            "initial_state": "available",
            "members": []
        });
        assert!(serde_json::from_value::<ManifestDefinition>(value).is_err());
    }
}
