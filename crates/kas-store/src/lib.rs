//! SQLite persistence for KAS's single-Resource model.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use kas_auth::{issue_token, token_hash, AuthContext, IssuedCredential, Rule, Subject};
use kas_core::{
    package_path_for_digest, ActionSpec, CreateResource, CreateRun, CredentialSpec, DeliveryStatus,
    DriverDelivery, DriverObservation, DriverReady, DriverSpec, DriverState, DriverWork, Event,
    EventFilter, EventType, FinishRun, KasMetadata, LinkSpec, ManifestDefinition, ManifestSpec,
    Mutation, PackageDefinition, PackageExpansion, PackageSpec, PlannedResource, RelationRole,
    RelationSpec, Resource, ResourceDefinition, ResourceMetadata, ResourceStatus,
    ResourceStatusMetadata, RestartPolicy, RoleSpec, RunResult, RunSpec, RunState, SystemRole,
    UpdateResource, UpdateResourceStatus, UserSpec, BUILTIN_PACKAGE_MEDIA_TYPE, STATE_AVAILABLE,
    STATE_DELETED,
};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const LATEST_SCHEMA_VERSION: u32 = 14;

pub const MANIFEST_MANIFEST: &str = "/builtin/manifest";
pub const ACTION_MANIFEST: &str = "/builtin/action";
pub const RELATION_MANIFEST: &str = "/builtin/relation";
pub const LINK_MANIFEST: &str = "/builtin/link";
pub const DRIVER_MANIFEST: &str = "/builtin/driver";
pub const RUN_MANIFEST: &str = "/builtin/run";
pub const USER_MANIFEST: &str = "/builtin/user";
pub const SERVICE_ACCOUNT_MANIFEST: &str = "/builtin/service-account";
pub const ROLE_MANIFEST: &str = "/builtin/role";
pub const ROLE_BINDING_RELATION: &str = "/builtin/relations/role-binding";
pub const CREDENTIAL_MANIFEST: &str = "/builtin/credential";
pub const PACKAGE_MANIFEST: &str = "/builtin/package";

const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_initial.sql")),
    (2, include_str!("../migrations/0002_reconciliations.sql")),
    (3, include_str!("../migrations/0003_rbac.sql")),
    (
        4,
        include_str!("../migrations/0004_events_and_deliveries.sql"),
    ),
    (5, include_str!("../migrations/0005_paths.sql")),
    (6, include_str!("../migrations/0006_link_endpoint_rbac.sql")),
    (
        7,
        include_str!("../migrations/0007_manifest_packages_and_relations.sql"),
    ),
    (
        8,
        include_str!("../migrations/0008_resource_and_link_reconciliation.sql"),
    ),
    (
        9,
        include_str!("../migrations/0009_single_resource_primitive.sql"),
    ),
    (
        10,
        include_str!("../migrations/0010_resource_metadata_and_driver_watches.sql"),
    ),
    (
        11,
        include_str!("../migrations/0011_active_driver_deliveries.sql"),
    ),
    (
        12,
        include_str!("../migrations/0012_role_binding_links.sql"),
    ),
    (
        13,
        include_str!("../migrations/0013_resources_and_events_only.sql"),
    ),
    (
        14,
        include_str!("../migrations/0014_resource_documents.sql"),
    ),
];

const RESOURCE_SELECT: &str = "SELECT path,metadata,spec,status FROM resources";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("database migration required: current version {current}, latest version {latest}")]
    MigrationRequired { current: u32, latest: u32 },
    #[error("database schema version {current} is newer than supported version {latest}")]
    UnsupportedSchema { current: u32, latest: u32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DriverLaunchConfig {
    pub package: Resource,
    pub entrypoint: String,
    pub args: Vec<String>,
    pub restart: RestartPolicy,
}

struct PackageInstallation {
    expansion: PackageExpansion,
    size_bytes: u64,
    media_type: String,
}

pub fn migrate(path: impl AsRef<Path>) -> Result<u32, StoreError> {
    let mut connection = Connection::open(path)?;
    configure(&connection)?;
    migrate_connection(&mut connection)
}

pub struct Store {
    connection: Connection,
    deliveries: HashMap<Uuid, DriverDelivery>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        configure(&connection)?;
        require_current_schema(&connection)?;
        let mut store = Self {
            connection,
            deliveries: HashMap::new(),
        };
        store.ensure_builtins()?;
        reconcile_platform_state(&mut store)?;
        Ok(store)
    }

    pub fn memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        configure(&connection)?;
        let mut store = Self {
            connection,
            deliveries: HashMap::new(),
        };
        migrate_connection(&mut store.connection)?;
        store.ensure_builtins()?;
        reconcile_platform_state(&mut store)?;
        Ok(store)
    }

    fn ensure_builtins(&mut self) -> Result<(), StoreError> {
        let mut packages = Vec::new();
        for documents in builtin_documents() {
            let digest = digest_documents(documents.manifest, documents.resources);
            let manifest: ManifestDefinition = serde_json::from_str(documents.manifest)?;
            let resources = documents
                .resources
                .iter()
                .map(|raw| serde_json::from_str::<ResourceDefinition>(raw))
                .collect::<Result<Vec<_>, _>>()?;
            let expansion = PackageDefinition {
                manifest,
                resources,
            }
            .expand(digest)
            .map_err(|error| StoreError::Invalid(format!("invalid built-in: {error}")))?;
            packages.push(PackageInstallation {
                expansion,
                size_bytes: documents.manifest.len() as u64
                    + documents
                        .resources
                        .iter()
                        .map(|raw| raw.len() as u64)
                        .sum::<u64>(),
                media_type: BUILTIN_PACKAGE_MEDIA_TYPE.into(),
            });
        }
        self.install_packages(packages)?;
        Ok(())
    }

    pub fn install_package(
        &mut self,
        expansion: PackageExpansion,
        package_size: u64,
        media_type: &str,
    ) -> Result<Resource, StoreError> {
        self.install_packages(vec![PackageInstallation {
            expansion,
            size_bytes: package_size,
            media_type: media_type.into(),
        }])?
        .into_iter()
        .next()
        .ok_or_else(|| StoreError::Invalid("Manifest package is empty".into()))
    }

    fn install_packages(
        &mut self,
        installations: Vec<PackageInstallation>,
    ) -> Result<Vec<Resource>, StoreError> {
        if installations.is_empty() {
            return Ok(Vec::new());
        }

        let mut roots = Vec::with_capacity(installations.len());
        let mut package_paths = Vec::with_capacity(installations.len());
        let mut existing_packages = Vec::with_capacity(installations.len());
        let mut existing_count = 0;
        for installation in &installations {
            let root = installation
                .expansion
                .resources
                .first()
                .cloned()
                .ok_or_else(|| StoreError::Invalid("Manifest package is empty".into()))?;
            if root.manifest != MANIFEST_MANIFEST && root.path != MANIFEST_MANIFEST {
                return Err(StoreError::Invalid(
                    "package root must be a Manifest Resource".into(),
                ));
            }
            let _: ManifestSpec = decode(&root.spec, "Manifest spec")?;
            if installation.expansion.resource_owners.get(&root.path) != Some(&root.path) {
                return Err(StoreError::Invalid("package root must own itself".into()));
            }
            let package_path = package_path_for_digest(&installation.expansion.artifact_digest)
                .ok_or_else(|| StoreError::Invalid("Package digest must be sha256 hex".into()))?;
            if self.get_resource(&root.path).is_ok() {
                existing_count += 1;
                let package = self.package_for_manifest(&root.path)?;
                if package.path != package_path
                    && installation.media_type != BUILTIN_PACKAGE_MEDIA_TYPE
                {
                    return Err(StoreError::Conflict(format!(
                        "Manifest {} is already installed",
                        root.path
                    )));
                }
                existing_packages.push(package);
            }
            roots.push(root);
            package_paths.push(package_path);
        }
        if existing_count == installations.len() {
            return Ok(existing_packages);
        }
        if existing_count != 0 {
            return Err(StoreError::Conflict(
                "built-in Manifest installation is incomplete".into(),
            ));
        }

        let tx = self.connection.transaction()?;
        let now = Utc::now();
        let mut stored_paths = Vec::new();
        let mut projections = Vec::new();

        // Install every Manifest root before packaged Resources. This resolves the
        // self-describing bootstrap cycle while keeping each definition and
        // Package independent.
        for roots_pass in [true, false] {
            for (installation, root) in installations.iter().zip(&roots) {
                for planned in &installation.expansion.resources {
                    let owner_manifest = installation
                        .expansion
                        .resource_owners
                        .get(&planned.path)
                        .ok_or_else(|| {
                            StoreError::Invalid(format!(
                                "Resource {} has no owning Manifest",
                                planned.path
                            ))
                        })?;
                    if (planned.path == *owner_manifest) != roots_pass {
                        continue;
                    }
                    validate_resource_identity(planned)?;
                    let planned = normalized_initial_documents(&tx, planned)?;
                    insert_resource_row(
                        &tx,
                        &planned,
                        true,
                        &format!("package:{}", root.path),
                        now,
                    )?;
                    stored_paths.push(planned.path.clone());
                    projections.push((planned, owner_manifest.clone()));
                }
            }
        }
        projections.sort_by_key(|(planned, _)| match planned.manifest.as_str() {
            MANIFEST_MANIFEST => 0,
            LINK_MANIFEST => 2,
            RUN_MANIFEST => 3,
            _ => 1,
        });
        for (planned, owner_manifest) in &projections {
            project_resource(&tx, planned, owner_manifest, now)?;
        }
        let stored_resources = stored_paths
            .iter()
            .map(|path| resource_in(&tx, path).map(planned_from_resource))
            .collect::<Result<Vec<_>, _>>()?;

        let mut package_resources = Vec::with_capacity(installations.len());
        for ((installation, root), package_path) in
            installations.iter().zip(&roots).zip(&package_paths)
        {
            let package_spec = PackageSpec {
                digest: installation.expansion.artifact_digest.clone(),
                size_bytes: installation.size_bytes,
                media_type: installation.media_type.clone(),
            };
            let package = PlannedResource {
                path: package_path.clone(),
                metadata: kas_core::PlannedResourceMetadata {
                    manifest: PACKAGE_MANIFEST.into(),
                    name: package_spec.digest.clone(),
                    state: String::new(),
                },
                spec: serde_json::to_value(&package_spec)?,
                status: ResourceStatus {
                    metadata: ResourceStatusMetadata::default(),
                    spec: serde_json::to_value(&package_spec)?,
                },
            };
            validate_resource_identity(&package)?;
            let package = normalized_initial_documents(&tx, &package)?;
            insert_resource_row(&tx, &package, true, &format!("package:{}", root.path), now)?;
            project_resource(&tx, &package, &root.path, now)?;
            let package_resource = resource_in(&tx, &package.path)?;
            append_event(&tx, EventType::Created, &package_resource, now)?;
            package_resources.push(package_resource);
        }

        // Relationships declared by the installed definitions are ordinary
        // protected Link Resources.
        if relation_path_for_role(&tx, RelationRole::PackageManifest)?.is_some() {
            for ((root, package), package_path) in
                roots.iter().zip(&package_resources).zip(&package_paths)
            {
                create_system_link(
                    &tx,
                    &format!("{package_path}/links/manifest"),
                    RelationRole::PackageManifest,
                    &package.path,
                    &root.path,
                    now,
                )?;
            }
        }
        for installation in &installations {
            for packaged_resource in installation.expansion.resources.iter().filter(|resource| {
                installation
                    .expansion
                    .resource_owners
                    .get(&resource.path)
                    .is_some_and(|owner| owner != &resource.path)
            }) {
                let owner = installation
                    .expansion
                    .resource_owners
                    .get(&packaged_resource.path)
                    .expect("filtered Resources have an owner");
                if relation_path_for_role(&tx, RelationRole::ManifestResource)?.is_some() {
                    create_system_link(
                        &tx,
                        &format!("{}/links/manifest-resource", packaged_resource.path),
                        RelationRole::ManifestResource,
                        owner,
                        &packaged_resource.path,
                        now,
                    )?;
                }
            }
        }
        for resource in &stored_resources {
            project_declared_relationships(&tx, resource, now)?;
        }
        reconcile_all_resources(&tx, "package_registered", now)?;
        tx.commit()?;
        Ok(package_resources)
    }

    pub fn create_resource(&mut self, input: CreateResource) -> Result<Resource, StoreError> {
        validate_resource_identity(&input)?;
        if input.manifest == PACKAGE_MANIFEST {
            return Err(StoreError::Invalid(
                "Package Resources can only be created by POST /packages".into(),
            ));
        }
        if input.manifest == CREDENTIAL_MANIFEST {
            return Err(StoreError::Invalid(
                "Credential Resources can only be created by POST /credentials".into(),
            ));
        }
        let tx = self.connection.transaction()?;
        let now = Utc::now();
        let input = normalized_initial_documents(&tx, &input)?;
        validate_against_manifest(
            &tx,
            &input.manifest,
            &input.metadata.state,
            &input.spec,
            &input.status,
        )?;
        insert_resource_row(&tx, &input, false, "user", now)?;
        project_resource(&tx, &input, "", now)?;
        let resource = tx.query_row(
            &format!("{RESOURCE_SELECT} WHERE path=?"),
            [&input.path],
            resource_from_row,
        )?;
        project_declared_relationships(&tx, &planned_from_resource(resource.clone()), now)?;
        append_event(&tx, EventType::Created, &resource, now)?;
        enqueue_if_drifted(&tx, &resource, "created", now)?;
        tx.commit()?;
        Ok(resource)
    }

    pub fn list_resources(&self, manifest: Option<&str>) -> Result<Vec<Resource>, StoreError> {
        let sql = if manifest.is_some() {
            format!(
                "{RESOURCE_SELECT}
                 WHERE json_extract(metadata,'$.manifest')=?
                 ORDER BY json_extract(metadata,'$.\"[kas]\".created_at'),path"
            )
        } else {
            format!(
                "{RESOURCE_SELECT}
                 ORDER BY json_extract(metadata,'$.\"[kas]\".created_at'),path"
            )
        };
        let mut statement = self.connection.prepare(&sql)?;
        if let Some(manifest) = manifest {
            let rows = statement.query_map([manifest], resource_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        } else {
            let rows = statement.query_map([], resource_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        }
    }

    pub fn get_resource(&self, path: &str) -> Result<Resource, StoreError> {
        self.connection
            .query_row(
                &format!("{RESOURCE_SELECT} WHERE path=?"),
                [path],
                resource_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Resource {path}")))
    }

    pub fn update_resource(
        &mut self,
        path: &str,
        input: UpdateResource,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut current = resource_in(&tx, path)?;
        if current.manifest == CREDENTIAL_MANIFEST {
            return Err(StoreError::Invalid(
                "Credential Resources can only be changed through credential endpoints".into(),
            ));
        }
        let state = input
            .metadata
            .as_ref()
            .map(|metadata| metadata.state.as_str())
            .unwrap_or(&current.metadata.state);
        validate_against_manifest(&tx, &current.manifest, state, &input.spec, &current.status)?;
        if current.revision != input.expected_revision || current.metadata.kas.protected {
            return Err(StoreError::Conflict(format!(
                "Resource {path} revision is stale or protected"
            )));
        }
        let now = Utc::now();
        current.spec = input.spec;
        current.metadata.state = state.into();
        current.metadata.kas.revision += 1;
        current.metadata.kas.updated_at = now;
        save_resource_in(&tx, &current)?;
        let resource = resource_in(&tx, path)?;
        refresh_projection(&tx, &resource, now)?;
        append_event(&tx, EventType::Updated, &resource, now)?;
        enqueue_if_drifted(&tx, &resource, "spec_updated", now)?;
        tx.commit()?;
        Ok(resource)
    }

    pub fn update_resource_status(
        &mut self,
        path: &str,
        driver_path: &str,
        generation: u64,
        mut input: UpdateResourceStatus,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        assert_driver_owns(&tx, path, driver_path, generation)?;
        let current = resource_in(&tx, path)?;
        normalize_submitted_status(&current, &mut input.status);
        validate_against_manifest(
            &tx,
            &current.manifest,
            &current.metadata.state,
            &current.spec,
            &input.status,
        )?;
        let now = Utc::now();
        if current.revision != input.expected_revision {
            return Err(StoreError::Conflict("Resource revision is stale".into()));
        }
        let mut current = current;
        current.status = input.status;
        save_resource_in(&tx, &current)?;
        let resource = resource_in(&tx, path)?;
        refresh_projection(&tx, &resource, now)?;
        append_event(&tx, EventType::Updated, &resource, now)?;
        maybe_finish_deleted_resource(&tx, path, now)?;
        tx.commit()?;
        Ok(resource)
    }

    pub fn delete_resource(
        &mut self,
        path: &str,
        expected_revision: u64,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut resource = resource_in(&tx, path)?;
        if resource.revision != expected_revision {
            return Err(StoreError::Conflict(format!(
                "Resource {path} revision is stale"
            )));
        }
        if resource.metadata.kas.protected {
            return Err(StoreError::Conflict(format!(
                "Resource {path} is protected"
            )));
        }
        let now = Utc::now();
        resource.metadata.state = STATE_DELETED.into();
        resource.metadata.kas.revision += 1;
        resource.metadata.kas.updated_at = now;
        save_resource_in(&tx, &resource)?;
        resource = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &resource, now)?;
        enqueue_if_drifted(&tx, &resource, "delete_requested", now)?;
        if driver_for_resource(&tx, &resource)?.is_none() {
            let mut status = resource.status.clone();
            let actual = status.metadata.kas.observed.clone();
            status.metadata = resource.metadata.clone();
            status.metadata.kas.observed = actual;
            status.spec = resource.spec.clone();
            resource.status = status;
            save_resource_in(&tx, &resource)?;
            maybe_finish_deleted_resource(&tx, path, now)?;
        }
        tx.commit()?;
        Ok(resource)
    }

    pub fn links_for_resource(&self, path: &str) -> Result<Vec<Resource>, StoreError> {
        self.list_links(Some(path), None, None, true)
    }

    pub fn list_links(
        &self,
        source: Option<&str>,
        relation: Option<&str>,
        target: Option<&str>,
        either_endpoint: bool,
    ) -> Result<Vec<Resource>, StoreError> {
        Ok(self
            .list_resources(Some(LINK_MANIFEST))?
            .into_iter()
            .filter(|resource| {
                let Ok(link) = decode::<LinkSpec>(&resource.spec, "Link spec") else {
                    return false;
                };
                source.is_none_or(|expected| {
                    link.source == expected || (either_endpoint && link.target == expected)
                }) && relation.is_none_or(|expected| link.relation == expected)
                    && target.is_none_or(|expected| {
                        link.target == expected || (either_endpoint && link.source == expected)
                    })
            })
            .collect())
    }

    fn relation_path(&self, role: RelationRole) -> Result<Option<String>, StoreError> {
        for relation in self.list_resources(Some(RELATION_MANIFEST))? {
            let spec: RelationSpec = decode(&relation.spec, "Relation spec")?;
            if spec.role == Some(role) {
                return Ok(Some(relation.path.clone()));
            }
        }
        Ok(None)
    }

    pub fn get_link(&self, path: &str) -> Result<Resource, StoreError> {
        let resource = self.get_resource(path)?;
        require_manifest(&resource, LINK_MANIFEST)?;
        Ok(resource)
    }

    pub fn current_event_sequence(&self) -> Result<u64, StoreError> {
        self.connection
            .query_row("SELECT COALESCE(MAX(sequence),0) FROM events", [], |row| {
                row.get(0)
            })
            .map_err(StoreError::from)
    }

    pub fn list_events_filtered(&self, filter: EventFilter) -> Result<Vec<Event>, StoreError> {
        let limit = filter.limit.unwrap_or(100).clamp(1, 1000);
        let mut statement = self.connection.prepare(
            "SELECT sequence,event_type,resource_path,revision,value_json,created_at
             FROM events
             WHERE (?1 IS NULL OR resource_path=?1) AND sequence>?2
             ORDER BY sequence LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                filter.resource_path,
                filter.after_sequence.unwrap_or(0),
                limit
            ],
            event_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn list_events(&self, after: Option<u64>, limit: usize) -> Result<Vec<Event>, StoreError> {
        self.list_events_filtered(EventFilter {
            after_sequence: after,
            limit: Some(limit),
            ..EventFilter::default()
        })
    }
}

impl Store {
    pub fn driver_for_manifest(&self, manifest_path: &str) -> Result<Option<Resource>, StoreError> {
        for driver in self.list_drivers()? {
            let spec: DriverSpec = decode(&driver.spec, "Driver spec")?;
            if spec.manages.iter().any(|managed| managed == manifest_path) {
                return Ok(Some(driver));
            }
        }
        Ok(None)
    }

    pub fn driver_launch_config(
        &self,
        driver_path: &str,
    ) -> Result<DriverLaunchConfig, StoreError> {
        let driver = self.get_driver(driver_path)?;
        let spec: DriverSpec = decode(&driver.spec, "Driver spec")?;
        let package = self.package_for_driver(driver_path)?;
        Ok(DriverLaunchConfig {
            package,
            entrypoint: spec.entrypoint,
            args: spec.args,
            restart: spec.restart,
        })
    }

    pub fn get_driver(&self, path: &str) -> Result<Resource, StoreError> {
        let resource = self.get_resource(path)?;
        require_manifest(&resource, DRIVER_MANIFEST)?;
        Ok(resource)
    }

    pub fn list_drivers(&self) -> Result<Vec<Resource>, StoreError> {
        self.list_resources(Some(DRIVER_MANIFEST))
    }

    pub fn start_driver(&mut self, path: &str) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut driver = resource_in(&tx, path)?;
        require_manifest(&driver, DRIVER_MANIFEST)?;
        let state = driver_state(&driver)?;
        if !matches!(state, DriverState::Stopped | DriverState::Failed) {
            return Err(StoreError::Conflict(format!(
                "Driver {path} cannot start from {:?}",
                state
            )));
        }
        let now = Utc::now();
        if driver.metadata.state != "running" {
            driver.metadata.state = "running".into();
            driver.metadata.kas.revision += 1;
            driver.metadata.kas.updated_at = now;
            save_resource_in(&tx, &driver)?;
            driver = resource_in(&tx, path)?;
            reconcile_all_resources(&tx, "driver_started", now)?;
        }
        let generation = driver.metadata.kas.generation + 1;
        driver.metadata.kas.generation = generation;
        save_resource_in(&tx, &driver)?;
        update_status_document(&tx, path, DriverState::Starting, &driver.status.spec, now)?;
        driver = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &driver, now)?;
        tx.commit()?;
        self.deliveries
            .retain(|_, delivery| delivery.driver_path != path);
        Ok(driver)
    }

    pub fn mark_driver_ready(
        &mut self,
        path: &str,
        ready: DriverReady,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut driver = resource_in(&tx, path)?;
        if driver.metadata.kas.generation != ready.generation
            || driver_state(&driver)? != DriverState::Starting
        {
            return Err(StoreError::Conflict("Driver generation is stale".into()));
        }
        let now = Utc::now();
        update_status_document(&tx, path, DriverState::Running, &driver.spec, now)?;
        driver = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &driver, now)?;
        tx.commit()?;
        Ok(driver)
    }

    pub fn heartbeat_driver(
        &mut self,
        path: &str,
        generation: u64,
    ) -> Result<Resource, StoreError> {
        let driver = self.get_driver(path)?;
        if self.driver_generation(path)? != generation
            || driver_state(&driver)? != DriverState::Running
        {
            return Err(StoreError::Conflict("Driver generation is stale".into()));
        }
        Ok(driver)
    }

    pub fn stop_driver(&mut self, path: &str) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut driver = resource_in(&tx, path)?;
        require_manifest(&driver, DRIVER_MANIFEST)?;
        let state = driver_state(&driver)?;
        let now = Utc::now();
        if driver.metadata.state != "stopped" {
            driver.metadata.state = "stopped".into();
            driver.metadata.kas.revision += 1;
            driver.metadata.kas.updated_at = now;
            save_resource_in(&tx, &driver)?;
            driver = resource_in(&tx, path)?;
            reconcile_all_resources(&tx, "driver_stopped", now)?;
        }
        let state = if matches!(state, DriverState::Starting | DriverState::Running) {
            DriverState::Stopping
        } else {
            state
        };
        update_status_document(&tx, path, state, &driver.status.spec, now)?;
        driver = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &driver, now)?;
        tx.commit()?;
        Ok(driver)
    }

    pub fn mark_driver_failed(
        &mut self,
        path: &str,
        generation: u64,
        error: impl Into<String>,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut driver = resource_in(&tx, path)?;
        if driver.metadata.kas.generation != generation {
            return Err(StoreError::Conflict("Driver generation is stale".into()));
        }
        let error = error.into();
        let now = Utc::now();
        update_status_document(&tx, path, DriverState::Failed, &driver.status.spec, now)?;
        eprintln!("Driver {path} failed: {error}");
        driver = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &driver, now)?;
        tx.commit()?;
        Ok(driver)
    }

    pub fn mark_driver_stopped(
        &mut self,
        path: &str,
        generation: u64,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut driver = resource_in(&tx, path)?;
        if driver.metadata.kas.generation != generation {
            return Err(StoreError::Conflict("Driver generation is stale".into()));
        }
        let now = Utc::now();
        update_status_document(&tx, path, DriverState::Stopped, &driver.spec, now)?;
        driver = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &driver, now)?;
        tx.commit()?;
        Ok(driver)
    }

    pub fn driver_generation(&self, path: &str) -> Result<u64, StoreError> {
        Ok(self.get_driver(path)?.metadata.kas.generation)
    }

    pub fn enqueue_run(&mut self, input: CreateRun) -> Result<Resource, StoreError> {
        let target = self.get_resource(&input.resource)?;
        let action = self.get_resource(&input.action)?;
        require_manifest(&action, ACTION_MANIFEST)?;
        let action_spec: ActionSpec = decode(&action.spec, "Action spec")?;
        validate_json_schema("Run input", &action_spec.input_schema, &input.input)?;
        let driver = self
            .driver_for_manifest(&target.manifest)?
            .ok_or_else(|| StoreError::Invalid("Resource Manifest has no Driver".into()))?;
        let spec = serde_json::to_value(RunSpec {
            request_id: input.request_id,
            resource: input.resource,
            action: input.action,
            driver: Some(driver.path.clone()),
            driver_generation: None,
            started_at: None,
            finished_at: None,
            input: input.input,
            output: None,
            error: None,
        })?;
        let status = ResourceStatus {
            metadata: ResourceStatusMetadata {
                state: run_state_name(RunState::Queued).into(),
                ..Default::default()
            },
            spec: spec.clone(),
        };
        self.create_resource(PlannedResource {
            path: input.path,
            metadata: kas_core::PlannedResourceMetadata {
                manifest: RUN_MANIFEST.into(),
                name: input.request_id.to_string(),
                state: run_state_name(RunState::Queued).into(),
            },
            spec,
            status,
        })
    }

    pub fn get_run(&self, path: &str) -> Result<Resource, StoreError> {
        let resource = self.get_resource(path)?;
        require_manifest(&resource, RUN_MANIFEST)?;
        Ok(resource)
    }

    pub fn finish_run(&mut self, run_path: &str, input: FinishRun) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut run = resource_in(&tx, run_path)?;
        require_manifest(&run, RUN_MANIFEST)?;
        let mut spec: RunSpec = decode(&run.spec, "Run spec")?;
        if spec.driver_generation != Some(input.driver_generation)
            || run_state(&run)? != RunState::Running
        {
            return Err(StoreError::Conflict("Run generation is stale".into()));
        }
        let state = apply_run_result(&mut spec, input.result);
        let now = Utc::now();
        spec.finished_at = Some(now);
        update_platform_resource(&tx, run_path, state, &spec, now)?;
        run = resource_in(&tx, run_path)?;
        append_event(&tx, EventType::Updated, &run, now)?;
        tx.commit()?;
        Ok(run)
    }

    pub fn finish_run_delivery_with_mutations(
        &mut self,
        delivery_id: Uuid,
        driver_path: &str,
        generation: u64,
        operations: Vec<Mutation>,
    ) -> Result<Vec<Value>, StoreError> {
        self.finish_delivery_with_mutations(delivery_id, driver_path, generation, operations, true)
    }

    pub fn finish_reconciliation_with_mutations(
        &mut self,
        delivery_id: Uuid,
        driver_path: &str,
        generation: u64,
        operations: Vec<Mutation>,
    ) -> Result<Vec<Value>, StoreError> {
        self.finish_delivery_with_mutations(delivery_id, driver_path, generation, operations, false)
    }

    fn finish_delivery_with_mutations(
        &mut self,
        delivery_id: Uuid,
        driver_path: &str,
        generation: u64,
        operations: Vec<Mutation>,
        _run_delivery: bool,
    ) -> Result<Vec<Value>, StoreError> {
        let delivery = self
            .deliveries
            .get(&delivery_id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(format!("Driver delivery {delivery_id}")))?;
        let tx = self.connection.transaction()?;
        if delivery.driver_path != driver_path || delivery.generation != generation {
            return Err(StoreError::Conflict("Delivery is stale".into()));
        }
        if delivery.status != DeliveryStatus::Acked {
            return Err(StoreError::Conflict("Delivery must be acknowledged".into()));
        }
        let now = Utc::now();
        let mut results = Vec::new();
        for operation in operations {
            results.push(apply_mutation(
                &tx,
                driver_path,
                generation,
                operation,
                now,
            )?);
        }
        match delivery.work {
            DriverWork::Reconcile {
                driver_revision,
                resource,
            } => {
                if let Ok(mut current) = resource_in(&tx, &resource.path) {
                    current.status.metadata.kas.observed.insert(
                        driver_path.into(),
                        DriverObservation {
                            driver_revision,
                            resource_revision: resource.revision,
                        },
                    );
                    save_resource_in(&tx, &current)?;
                }
                if let Ok(current) = resource_in(&tx, &resource.path) {
                    enqueue_if_drifted(&tx, &current, "delivery_completed", now)?;
                    maybe_finish_deleted_resource(&tx, &resource.path, now)?;
                }
            }
            DriverWork::Run { .. } => {}
        }
        tx.commit()?;
        self.deliveries.remove(&delivery_id);
        Ok(results)
    }
}

impl Store {
    pub fn claim_driver_delivery(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Option<DriverDelivery>, StoreError> {
        let tx = self.connection.transaction()?;
        assert_driver_generation(&tx, driver_path, generation)?;

        if let Some(delivery) = self
            .deliveries
            .values()
            .find(|delivery| {
                delivery.driver_path == driver_path && delivery.generation == generation
            })
            .cloned()
        {
            tx.commit()?;
            return Ok(Some(delivery));
        }

        let now = Utc::now();
        let reconcile = next_reconciliation_in(&tx, driver_path)?;
        let work = if let Some((path, driver_revision)) = reconcile {
            Some(DriverWork::Reconcile {
                driver_revision,
                resource: resource_in(&tx, &path)?,
            })
        } else {
            let run_path: Option<String> = tx
                .query_row(
                    "SELECT path FROM resources
                     WHERE json_extract(metadata,'$.manifest')='/builtin/run'
                       AND json_extract(spec,'$.driver')=?
                       AND json_extract(status,'$.metadata.state') IN ('queued','running')
                     ORDER BY json_extract(metadata,'$.\"[kas]\".created_at'),path LIMIT 1",
                    [driver_path],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(path) = run_path {
                let mut run = resource_in(&tx, &path)?;
                let mut spec: RunSpec = decode(&run.spec, "Run spec")?;
                if spec.driver_generation != Some(generation) || spec.started_at.is_none() {
                    spec.driver_generation = Some(generation);
                    spec.started_at = Some(now);
                    spec.finished_at = None;
                    update_platform_resource(&tx, &path, RunState::Running, &spec, now)?;
                    append_event(&tx, EventType::Updated, &resource_in(&tx, &path)?, now)?;
                }
                run = resource_in(&tx, &path)?;
                Some(DriverWork::Run {
                    run,
                    resource: resource_in(&tx, &spec.resource)?,
                    action: resource_in(&tx, &spec.action)?,
                })
            } else {
                None
            }
        };
        let Some(work) = work else {
            tx.commit()?;
            return Ok(None);
        };
        let delivery = DriverDelivery {
            id: Uuid::new_v4(),
            driver_path: driver_path.into(),
            generation,
            work,
            status: DeliveryStatus::Pending,
            created_at: now,
            acked_at: None,
            completed_at: None,
        };
        tx.commit()?;
        self.deliveries.insert(delivery.id, delivery.clone());
        Ok(Some(delivery))
    }

    pub fn acknowledge_driver_delivery(
        &mut self,
        id: Uuid,
        driver_path: &str,
        generation: u64,
    ) -> Result<DriverDelivery, StoreError> {
        let delivery = self
            .deliveries
            .get_mut(&id)
            .ok_or_else(|| StoreError::Conflict("Driver delivery is stale".into()))?;
        if delivery.driver_path != driver_path || delivery.generation != generation {
            return Err(StoreError::Conflict("Driver delivery is stale".into()));
        }
        delivery.status = DeliveryStatus::Acked;
        delivery.acked_at.get_or_insert_with(Utc::now);
        Ok(delivery.clone())
    }

    pub fn complete_driver_delivery(
        &mut self,
        id: Uuid,
        driver_path: &str,
        generation: u64,
    ) -> Result<DriverDelivery, StoreError> {
        let mut delivery = self
            .deliveries
            .remove(&id)
            .ok_or_else(|| StoreError::Conflict("Driver delivery is not acknowledged".into()))?;
        if delivery.driver_path != driver_path
            || delivery.generation != generation
            || delivery.status != DeliveryStatus::Acked
        {
            self.deliveries.insert(id, delivery);
            return Err(StoreError::Conflict(
                "Driver delivery is not acknowledged".into(),
            ));
        }
        delivery.status = DeliveryStatus::Completed;
        delivery.completed_at = Some(Utc::now());
        Ok(delivery)
    }

    pub fn get_driver_delivery(&self, id: Uuid) -> Result<DriverDelivery, StoreError> {
        self.deliveries
            .get(&id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(format!("Driver delivery {id}")))
    }

    pub fn pending_driver_deliveries(
        &self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Vec<DriverDelivery>, StoreError> {
        let mut deliveries = self
            .deliveries
            .values()
            .filter(|delivery| {
                delivery.driver_path == driver_path && delivery.generation == generation
            })
            .cloned()
            .collect::<Vec<_>>();
        deliveries.sort_by_key(|delivery| (delivery.created_at, delivery.id));
        Ok(deliveries)
    }

    pub fn manifest_for_driver(&self, driver_path: &str) -> Result<Resource, StoreError> {
        let relation = self.relation_path(RelationRole::ManifestResource)?;
        let link = self
            .list_links(None, relation.as_deref(), Some(driver_path), false)?
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::NotFound(format!("Manifest for Driver {driver_path}")))?;
        let spec: LinkSpec = decode(&link.spec, "ManifestResource Link spec")?;
        self.get_resource(&spec.source)
    }

    pub fn package_for_manifest(&self, manifest_path: &str) -> Result<Resource, StoreError> {
        let relation = self.relation_path(RelationRole::PackageManifest)?;
        let link = self
            .list_links(None, relation.as_deref(), Some(manifest_path), false)?
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::NotFound(format!("Package for Manifest {manifest_path}")))?;
        let spec: LinkSpec = decode(&link.spec, "PackageManifest Link spec")?;
        self.get_resource(&spec.source)
    }

    pub fn package_for_driver(&self, driver_path: &str) -> Result<Resource, StoreError> {
        let manifest = self.manifest_for_driver(driver_path)?;
        self.package_for_manifest(&manifest.path)
    }

    pub fn bootstrap_admin(&mut self, name: &str) -> Result<IssuedCredential, StoreError> {
        validate_name("User name", name)?;
        let path = format!("/users/{}", permission_segment(name));
        if self.get_resource(&path).is_err() {
            self.create_resource(PlannedResource {
                path: path.clone(),
                metadata: kas_core::PlannedResourceMetadata {
                    manifest: USER_MANIFEST.into(),
                    name: name.into(),
                    state: String::new(),
                },
                spec: serde_json::to_value(UserSpec { disabled: false })?,
                status: ResourceStatus {
                    metadata: ResourceStatusMetadata::default(),
                    spec: serde_json::to_value(UserSpec { disabled: false })?,
                },
            })?;
        }
        let admin_role = self
            .list_resources(Some(ROLE_MANIFEST))?
            .into_iter()
            .find(|role| {
                decode::<RoleSpec>(&role.spec, "Role spec")
                    .is_ok_and(|spec| spec.system_role == Some(SystemRole::Admin))
            })
            .ok_or_else(|| StoreError::NotFound("built-in admin Role".into()))?;
        let binding_path = format!("{path}/links/admin-role");
        if self.get_resource(&binding_path).is_err() {
            let spec = LinkSpec {
                relation: ROLE_BINDING_RELATION.into(),
                source: path.clone(),
                target: admin_role.path.clone(),
                metadata: json!({}),
            };
            self.create_resource(PlannedResource {
                path: binding_path,
                metadata: kas_core::PlannedResourceMetadata {
                    manifest: LINK_MANIFEST.into(),
                    name: format!("{name}-admin"),
                    state: String::new(),
                },
                spec: serde_json::to_value(&spec)?,
                status: ResourceStatus {
                    metadata: ResourceStatusMetadata::default(),
                    spec: serde_json::to_value(&spec)?,
                },
            })?;
        }
        self.issue_credential(&path, None)
    }

    pub fn issue_credential(
        &mut self,
        subject_path: &str,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<IssuedCredential, StoreError> {
        self.issue_credential_for_generation(subject_path, expires_at, None)
    }

    pub fn issue_driver_credential(
        &mut self,
        driver_path: &str,
    ) -> Result<IssuedCredential, StoreError> {
        let driver = self.get_driver(driver_path)?;
        let spec: DriverSpec = decode(&driver.spec, "Driver spec")?;
        self.issue_credential_for_generation(
            &spec.service_account,
            Some(Utc::now() + Duration::hours(1)),
            Some(self.driver_generation(driver_path)?),
        )
    }

    fn issue_credential_for_generation(
        &mut self,
        subject_path: &str,
        expires_at: Option<DateTime<Utc>>,
        driver_generation: Option<u64>,
    ) -> Result<IssuedCredential, StoreError> {
        let subject = self.get_resource(subject_path)?;
        if subject.manifest != USER_MANIFEST && subject.manifest != SERVICE_ACCOUNT_MANIFEST {
            return Err(StoreError::Invalid(
                "Credential subject must be a User or ServiceAccount Resource".into(),
            ));
        }
        let id = Uuid::new_v4();
        let credential_path = format!("{subject_path}/credentials/{id}");
        let token = issue_token();
        let spec = CredentialSpec {
            subject: subject_path.into(),
            token_hash: token_hash(&token),
            driver_generation,
            expires_at,
            revoked_at: None,
        };
        let tx = self.connection.transaction()?;
        let now = Utc::now();
        let planned = PlannedResource {
            path: credential_path.clone(),
            metadata: kas_core::PlannedResourceMetadata {
                manifest: CREDENTIAL_MANIFEST.into(),
                name: id.to_string(),
                state: String::new(),
            },
            spec: serde_json::to_value(&spec)?,
            status: ResourceStatus {
                metadata: ResourceStatusMetadata::default(),
                spec: serde_json::to_value(&spec)?,
            },
        };
        let planned = normalized_initial_documents(&tx, &planned)?;
        insert_resource_row(&tx, &planned, false, "system", now)?;
        let resource = resource_in(&tx, &credential_path)?;
        append_event(&tx, EventType::Created, &resource, now)?;
        tx.commit()?;
        Ok(IssuedCredential {
            resource_path: credential_path,
            token,
            expires_at,
        })
    }

    pub fn authenticate(&self, token: &str) -> Result<AuthContext, StoreError> {
        let hash = token_hash(token);
        let credential = self
            .connection
            .query_row(
                &format!(
                    "{RESOURCE_SELECT}
                     WHERE json_extract(metadata,'$.manifest')=?
                       AND json_extract(spec,'$.token_hash')=?"
                ),
                params![CREDENTIAL_MANIFEST, hash],
                resource_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Credential".into()))?;
        let spec: CredentialSpec = decode(&credential.spec, "Credential spec")?;
        if spec.expires_at.is_some_and(|expiry| expiry <= Utc::now()) || spec.revoked_at.is_some() {
            return Err(StoreError::NotFound("Credential".into()));
        }
        let subject_resource = self.get_resource(&spec.subject)?;
        if subject_resource.manifest == USER_MANIFEST {
            let user: UserSpec = decode(&subject_resource.spec, "User spec")?;
            if user.disabled {
                return Err(StoreError::NotFound("Credential".into()));
            }
        }

        let mut rules = Vec::new();
        for binding in self.list_links(
            Some(&spec.subject),
            Some(ROLE_BINDING_RELATION),
            None,
            false,
        )? {
            if binding.metadata.state == STATE_DELETED {
                continue;
            }
            let binding_spec: LinkSpec = decode(&binding.spec, "RoleBinding Link spec")?;
            let role = self.get_resource(&binding_spec.target)?;
            if role.manifest != ROLE_MANIFEST || role.metadata.state == STATE_DELETED {
                continue;
            }
            let role_spec: RoleSpec = decode(&role.spec, "Role spec")?;
            rules.extend(role_spec.rules.into_iter().map(|rule| Rule {
                manifests: rule.manifests,
                verbs: rule.verbs,
                paths: rule.paths,
            }));
        }
        let driver_path = if subject_resource.manifest == SERVICE_ACCOUNT_MANIFEST {
            self.list_drivers()?.into_iter().find_map(|driver| {
                decode::<DriverSpec>(&driver.spec, "Driver spec")
                    .ok()
                    .filter(|driver_spec| driver_spec.service_account == spec.subject)
                    .map(|_| driver.path.clone())
            })
        } else {
            None
        };
        Ok(AuthContext {
            credential_path: credential.path.clone(),
            subject: Subject {
                path: subject_resource.path.clone(),
                manifest: subject_resource.manifest.clone(),
            },
            rules,
            driver_path,
            driver_generation: spec.driver_generation,
        })
    }

    pub fn revoke_credential(&mut self, path: &str) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let resource = resource_in(&tx, path)?;
        require_manifest(&resource, CREDENTIAL_MANIFEST)?;
        let mut spec: CredentialSpec = decode(&resource.spec, "Credential spec")?;
        spec.revoked_at = Some(Utc::now());
        let now = Utc::now();
        update_platform_resource(&tx, path, "revoked", &spec, now)?;
        let resource = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &resource, now)?;
        tx.commit()?;
        Ok(resource)
    }
}

struct BuiltinDocuments {
    manifest: &'static str,
    resources: &'static [&'static str],
}

fn builtin_documents() -> [BuiltinDocuments; 11] {
    [
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/manifest/manifest.json"),
            resources: &[include_str!(
                "../../../builtins/manifest/resources/relations/manifest-resource.json"
            )],
        },
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/action/manifest.json"),
            resources: &[],
        },
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/relation/manifest.json"),
            resources: &[],
        },
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/link/manifest.json"),
            resources: &[
                include_str!("../../../builtins/link/resources/relations/role-binding.json"),
                include_str!("../../../builtins/link/resources/service-accounts/driver.json"),
                include_str!("../../../builtins/link/resources/roles/driver.json"),
                include_str!("../../../builtins/link/resources/links/driver.json"),
                include_str!("../../../builtins/link/resources/drivers/driver.json"),
            ],
        },
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/driver/manifest.json"),
            resources: &[include_str!(
                "../../../builtins/driver/resources/relations/driver-service-account.json"
            )],
        },
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/run/manifest.json"),
            resources: &[
                include_str!("../../../builtins/run/resources/relations/run-resource.json"),
                include_str!("../../../builtins/run/resources/relations/run-action.json"),
                include_str!("../../../builtins/run/resources/relations/run-driver.json"),
            ],
        },
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/user/manifest.json"),
            resources: &[],
        },
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/service-account/manifest.json"),
            resources: &[],
        },
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/role/manifest.json"),
            resources: &[
                include_str!("../../../builtins/role/resources/roles/admin.json"),
                include_str!("../../../builtins/role/resources/roles/editor.json"),
                include_str!("../../../builtins/role/resources/roles/viewer.json"),
            ],
        },
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/credential/manifest.json"),
            resources: &[],
        },
        BuiltinDocuments {
            manifest: include_str!("../../../builtins/package/manifest.json"),
            resources: &[include_str!(
                "../../../builtins/package/resources/relations/package-manifest.json"
            )],
        },
    ]
}

fn configure(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "PRAGMA foreign_keys=ON;
         PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;",
    )?;
    Ok(())
}

fn migrate_connection(connection: &mut Connection) -> Result<u32, StoreError> {
    let current = schema_version(connection)?;
    if current > LATEST_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            current,
            latest: LATEST_SCHEMA_VERSION,
        });
    }
    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        let rebuilds_resource_table = *version == 14;
        if rebuilds_resource_table {
            connection.pragma_update(None, "foreign_keys", false)?;
        }
        let migration = (|| -> Result<(), StoreError> {
            let tx = connection.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
            Ok(())
        })();
        if rebuilds_resource_table {
            connection.pragma_update(None, "foreign_keys", true)?;
        }
        migration?;
    }
    Ok(LATEST_SCHEMA_VERSION)
}

fn require_current_schema(connection: &Connection) -> Result<(), StoreError> {
    let current = schema_version(connection)?;
    match current.cmp(&LATEST_SCHEMA_VERSION) {
        std::cmp::Ordering::Less => Err(StoreError::MigrationRequired {
            current,
            latest: LATEST_SCHEMA_VERSION,
        }),
        std::cmp::Ordering::Greater => Err(StoreError::UnsupportedSchema {
            current,
            latest: LATEST_SCHEMA_VERSION,
        }),
        std::cmp::Ordering::Equal => Ok(()),
    }
}

fn schema_version(connection: &Connection) -> Result<u32, StoreError> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(StoreError::from)
}

fn digest_documents(manifest: &str, resources: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for document in std::iter::once(manifest).chain(resources.iter().copied()) {
        hasher.update((document.len() as u64).to_be_bytes());
        hasher.update(document.as_bytes());
    }
    let hex = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

fn resource_from_row(row: &Row<'_>) -> rusqlite::Result<Resource> {
    Ok(Resource {
        path: row.get(0)?,
        metadata: json_from_row(row, 1)?,
        spec: json_from_row(row, 2)?,
        status: json_from_row(row, 3)?,
    })
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    let event_type: String = row.get(1)?;
    Ok(Event {
        sequence: row.get(0)?,
        event_type: match event_type.as_str() {
            "created" => EventType::Created,
            "updated" => EventType::Updated,
            "deleted" => EventType::Deleted,
            other => {
                return Err(from_sql(
                    1,
                    std::io::Error::other(format!("invalid event type {other}")),
                ))
            }
        },
        resource_path: row.get(2)?,
        revision: row.get(3)?,
        value: json_from_row(row, 4)?,
        created_at: time_from_row(row, 5)?,
    })
}

fn resource_in(tx: &Transaction<'_>, path: &str) -> Result<Resource, StoreError> {
    tx.query_row(
        &format!("{RESOURCE_SELECT} WHERE path=?"),
        [path],
        resource_from_row,
    )
    .optional()?
    .ok_or_else(|| StoreError::NotFound(format!("Resource {path}")))
}

fn validate_resource_identity(resource: &PlannedResource) -> Result<(), StoreError> {
    kas_auth::validate_path(&resource.path)
        .map_err(|error| StoreError::Invalid(format!("invalid Resource path: {error}")))?;
    kas_auth::validate_path(&resource.manifest)
        .map_err(|error| StoreError::Invalid(format!("invalid Manifest path: {error}")))?;
    validate_name("Resource name", &resource.name)?;
    Ok(())
}

fn validate_name(kind: &str, name: &str) -> Result<(), StoreError> {
    if name.trim().is_empty() || name.len() > 255 {
        return Err(StoreError::Invalid(format!(
            "{kind} must contain 1 to 255 characters"
        )));
    }
    Ok(())
}

fn permission_segment(name: &str) -> String {
    let normalized = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    normalized.trim_matches('-').to_owned()
}

fn normalized_initial_documents(
    tx: &Transaction<'_>,
    planned: &PlannedResource,
) -> Result<PlannedResource, StoreError> {
    let mut planned = planned.clone();
    if planned.path == MANIFEST_MANIFEST && planned.manifest == MANIFEST_MANIFEST {
        let manifest_spec: ManifestSpec = decode(&planned.spec, "Manifest spec")?;
        normalize_resource_states(&mut planned, &manifest_spec);
        return Ok(planned);
    }
    let manifest = resource_in(tx, &planned.manifest)?;
    require_manifest(&manifest, MANIFEST_MANIFEST)?;
    let manifest_spec: ManifestSpec = decode(&manifest.spec, "Manifest spec")?;
    normalize_resource_states(&mut planned, &manifest_spec);
    Ok(planned)
}

fn normalize_resource_states(planned: &mut PlannedResource, manifest: &ManifestSpec) {
    if planned.metadata.state.is_empty() {
        planned.metadata.state = manifest.default_state.clone();
    }
    if planned.status.metadata.state.is_empty() {
        planned.status.metadata.state = manifest.initial_state.clone();
    }
    if planned.status.spec == json!({}) {
        planned.status.spec = planned.spec.clone();
    }
}

fn planned_from_resource(resource: Resource) -> PlannedResource {
    PlannedResource {
        path: resource.path,
        metadata: kas_core::PlannedResourceMetadata {
            manifest: resource.metadata.manifest,
            name: resource.metadata.name,
            state: resource.metadata.state,
        },
        spec: resource.spec,
        status: resource.status,
    }
}

fn validate_against_manifest(
    tx: &Transaction<'_>,
    manifest_path: &str,
    state: &str,
    spec: &Value,
    status: &ResourceStatus,
) -> Result<(), StoreError> {
    validate_no_reserved_keys("Resource spec", spec)?;
    validate_no_reserved_keys("Resource status spec", &status.spec)?;
    if manifest_path == MANIFEST_MANIFEST
        && tx
            .query_row(
                "SELECT 1 FROM resources WHERE path=?",
                [MANIFEST_MANIFEST],
                |_| Ok(()),
            )
            .optional()?
            .is_none()
    {
        return Ok(());
    }
    let manifest = resource_in(tx, manifest_path)?;
    require_manifest(&manifest, MANIFEST_MANIFEST)?;
    let definition: ManifestSpec = decode(&manifest.spec, "Manifest spec")?;
    validate_resource_state("Resource metadata", state, &definition)?;
    validate_resource_state(
        "Resource status metadata",
        &status.metadata.state,
        &definition,
    )?;
    validate_json_schema("Resource spec", &definition.resource_schema, spec)?;
    validate_json_schema(
        "Resource status spec",
        &definition.resource_schema,
        &status.spec,
    )
}

fn validate_no_reserved_keys(kind: &str, value: &Value) -> Result<(), StoreError> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key.contains('[') || key.contains(']') {
                    return Err(StoreError::Invalid(format!(
                        "{kind} field {key:?} contains reserved '[' or ']' characters"
                    )));
                }
                validate_no_reserved_keys(kind, child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                validate_no_reserved_keys(kind, child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_json_schema(kind: &str, schema: &Value, value: &Value) -> Result<(), StoreError> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| StoreError::Invalid(format!("{kind} schema is invalid: {error}")))?;
    if let Err(error) = validator.validate(value) {
        return Err(StoreError::Invalid(format!("{kind} is invalid: {error}")));
    }
    Ok(())
}

fn insert_resource_row(
    tx: &Transaction<'_>,
    planned: &PlannedResource,
    protected: bool,
    managed_by: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let metadata = ResourceMetadata {
        manifest: planned.manifest.clone(),
        name: planned.name.clone(),
        state: planned.metadata.state.clone(),
        kas: KasMetadata {
            revision: 0,
            generation: 0,
            observed: Default::default(),
            protected,
            managed_by: managed_by.into(),
            created_at: now,
            updated_at: now,
        },
    };
    let mut status = planned.status.clone();
    status.metadata.manifest = planned.manifest.clone();
    status.metadata.name = planned.name.clone();
    status.metadata.kas = KasMetadata {
        revision: 0,
        generation: 0,
        observed: Default::default(),
        protected,
        managed_by: managed_by.into(),
        created_at: now,
        updated_at: now,
    };
    validate_against_manifest(
        tx,
        &planned.manifest,
        &planned.metadata.state,
        &planned.spec,
        &status,
    )?;
    tx.execute(
        "INSERT INTO resources(path,metadata,spec,status) VALUES (?,?,?,?)",
        params![
            planned.path,
            serde_json::to_string(&metadata)?,
            serde_json::to_string(&planned.spec)?,
            serde_json::to_string(&status)?
        ],
    )
    .map_err(|error| constraint(error, "Resource already exists"))?;
    Ok(())
}

fn project_resource(
    tx: &Transaction<'_>,
    planned: &PlannedResource,
    _owner_manifest: &str,
    _now: DateTime<Utc>,
) -> Result<(), StoreError> {
    match planned.manifest.as_str() {
        MANIFEST_MANIFEST => {
            let spec: ManifestSpec = decode(&planned.spec, "Manifest spec")?;
            validate_manifest_states(&spec)?;
        }
        RELATION_MANIFEST => {}
        ACTION_MANIFEST => {
            let _: ActionSpec = decode(&planned.spec, "Action spec")?;
        }
        DRIVER_MANIFEST => {
            let spec: DriverSpec = decode(&planned.spec, "Driver spec")?;
            project_driver_manifests(tx, &planned.path, &spec)?;
        }
        LINK_MANIFEST => {}
        RUN_MANIFEST => {
            project_run(tx, planned)?;
        }
        ROLE_MANIFEST => {
            let _: RoleSpec = decode(&planned.spec, "Role spec")?;
        }
        CREDENTIAL_MANIFEST => {
            let _: CredentialSpec = decode(&planned.spec, "Credential spec")?;
        }
        PACKAGE_MANIFEST => {
            let _: PackageSpec = decode(&planned.spec, "Package spec")?;
        }
        USER_MANIFEST => {
            let _: UserSpec = decode(&planned.spec, "User spec")?;
        }
        SERVICE_ACCOUNT_MANIFEST => {}
        _ => {}
    }
    Ok(())
}

fn validate_manifest_states(spec: &ManifestSpec) -> Result<(), StoreError> {
    let mut declared = std::collections::BTreeSet::new();
    for state in &spec.states {
        if state.trim().is_empty() {
            return Err(StoreError::Invalid("Manifest state cannot be empty".into()));
        }
        if is_builtin_state(state) {
            return Err(StoreError::Invalid(format!(
                "Manifest must not redeclare built-in state {state}"
            )));
        }
        if !declared.insert(state.as_str()) {
            return Err(StoreError::Invalid(format!(
                "Manifest declares state {state} more than once"
            )));
        }
    }
    for (field, state) in [
        ("default_state", spec.default_state.as_str()),
        ("initial_state", spec.initial_state.as_str()),
    ] {
        if !is_builtin_state(state) && !declared.contains(state) {
            return Err(StoreError::Invalid(format!(
                "Manifest {field} {state} is neither built-in nor declared"
            )));
        }
    }
    Ok(())
}

fn project_driver_manifests(
    tx: &Transaction<'_>,
    driver_path: &str,
    spec: &DriverSpec,
) -> Result<(), StoreError> {
    if spec.manages.is_empty() {
        return Err(StoreError::Invalid(
            "Driver must manage at least one Manifest".into(),
        ));
    }
    let existing_drivers = resources_for_manifest_in(tx, DRIVER_MANIFEST)?;
    for existing in &existing_drivers {
        if existing.path == driver_path {
            continue;
        }
        let existing_spec: DriverSpec = decode(&existing.spec, "Driver spec")?;
        if existing_spec.service_account == spec.service_account {
            return Err(StoreError::Conflict(format!(
                "ServiceAccount {} already belongs to Driver {}",
                spec.service_account, existing.path
            )));
        }
    }

    let mut unique = std::collections::BTreeSet::new();
    for manifest_path in &spec.manages {
        if !unique.insert(manifest_path) {
            return Err(StoreError::Invalid(format!(
                "Driver manages Manifest {manifest_path} more than once"
            )));
        }
        let manifest = resource_in(tx, manifest_path)?;
        require_manifest(&manifest, MANIFEST_MANIFEST)?;
        for existing in &existing_drivers {
            if existing.path == driver_path {
                continue;
            }
            let existing_spec: DriverSpec = decode(&existing.spec, "Driver spec")?;
            if existing_spec
                .manages
                .iter()
                .any(|managed| managed == manifest_path)
            {
                return Err(StoreError::Conflict(format!(
                    "Manifest {manifest_path} already has a managing Driver"
                )));
            }
        }
    }
    Ok(())
}

fn validate_resource_state(
    kind: &str,
    state: &str,
    manifest: &ManifestSpec,
) -> Result<(), StoreError> {
    if state.is_empty() {
        return Err(StoreError::Invalid(format!("{kind} state cannot be empty")));
    }
    if is_builtin_state(state) || manifest.states.iter().any(|declared| declared == state) {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "{kind} state {state} is not allowed by its Manifest"
        )))
    }
}

fn is_builtin_state(state: &str) -> bool {
    matches!(
        state,
        kas_core::STATE_PENDING | STATE_AVAILABLE | STATE_DELETED
    )
}

fn project_run(tx: &Transaction<'_>, planned: &PlannedResource) -> Result<(), StoreError> {
    let mut spec: RunSpec = decode(&planned.spec, "Run spec")?;
    let target = resource_in(tx, &spec.resource)?;
    let action = resource_in(tx, &spec.action)?;
    require_manifest(&action, ACTION_MANIFEST)?;
    let action_spec: ActionSpec = decode(&action.spec, "Action spec")?;
    validate_json_schema("Run input", &action_spec.input_schema, &spec.input)?;
    let driver_path = if let Some(driver) = &spec.driver {
        driver.clone()
    } else {
        driver_path_for_manifest(tx, &target.manifest)?
            .ok_or_else(|| StoreError::Invalid("Resource Manifest has no Driver".into()))?
    };
    spec.driver = Some(driver_path.clone());
    let mut stored_spec = planned.spec.clone();
    stored_spec
        .as_object_mut()
        .ok_or_else(|| StoreError::Invalid("Run spec must be an object".into()))?
        .insert("driver".into(), Value::String(driver_path.clone()));
    stored_spec
        .as_object_mut()
        .expect("Run spec was checked as an object")
        .entry("driver_generation")
        .or_insert(Value::Null);
    let mut run = resource_in(tx, &planned.path)?;
    run.spec = stored_spec.clone();
    run.status.spec = stored_spec;
    save_resource_in(tx, &run)?;
    Ok(())
}

fn project_declared_relationships(
    tx: &Transaction<'_>,
    resource: &PlannedResource,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    match resource.manifest.as_str() {
        DRIVER_MANIFEST => {
            let spec: DriverSpec = decode(&resource.spec, "Driver spec")?;
            if relation_path_for_role(tx, RelationRole::DriverServiceAccount)?.is_some() {
                let link_path = format!("{}/links/service-account", resource.path);
                create_system_link(
                    tx,
                    &link_path,
                    RelationRole::DriverServiceAccount,
                    &resource.path,
                    &spec.service_account,
                    now,
                )?;
            }
        }
        RUN_MANIFEST => {
            let spec: RunSpec = decode(&resource.spec, "Run spec")?;
            for (role, target, suffix) in [
                (
                    RelationRole::RunResource,
                    spec.resource.as_str(),
                    "resource",
                ),
                (RelationRole::RunAction, spec.action.as_str(), "action"),
                (
                    RelationRole::RunDriver,
                    spec.driver
                        .as_deref()
                        .ok_or_else(|| StoreError::Invalid("Run has no Driver".into()))?,
                    "driver",
                ),
            ] {
                if relation_path_for_role(tx, role)?.is_some() {
                    create_system_link(
                        tx,
                        &format!("{}/links/{suffix}", resource.path),
                        role,
                        &resource.path,
                        target,
                        now,
                    )?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn create_system_link(
    tx: &Transaction<'_>,
    path: &str,
    role: RelationRole,
    source: &str,
    target: &str,
    now: DateTime<Utc>,
) -> Result<Option<Resource>, StoreError> {
    let Some(relation) = relation_path_for_role(tx, role)? else {
        return Ok(None);
    };
    let spec = LinkSpec {
        relation,
        source: source.to_owned(),
        target: target.to_owned(),
        metadata: json!({}),
    };
    let planned = PlannedResource {
        path: path.into(),
        metadata: kas_core::PlannedResourceMetadata {
            manifest: LINK_MANIFEST.into(),
            name: path.rsplit('/').next().unwrap_or("link").into(),
            state: String::new(),
        },
        spec: serde_json::to_value(&spec)?,
        status: ResourceStatus {
            metadata: ResourceStatusMetadata::default(),
            spec: serde_json::to_value(&spec)?,
        },
    };
    if tx
        .query_row("SELECT 1 FROM resources WHERE path=?", [path], |_| Ok(()))
        .optional()?
        .is_some()
    {
        return Ok(Some(resource_in(tx, path)?));
    }
    let planned = normalized_initial_documents(tx, &planned)?;
    insert_resource_row(tx, &planned, true, "system", now)?;
    let resource = resource_in(tx, path)?;
    append_event(tx, EventType::Created, &resource, now)?;
    enqueue_if_drifted(tx, &resource, "system_link_created", now)?;
    Ok(Some(resource))
}

fn relation_path_for_role(
    tx: &Transaction<'_>,
    role: RelationRole,
) -> Result<Option<String>, StoreError> {
    for relation in resources_for_manifest_in(tx, RELATION_MANIFEST)? {
        let spec: RelationSpec = decode(&relation.spec, "Relation spec")?;
        if spec.role == Some(role) {
            return Ok(Some(relation.path.clone()));
        }
    }
    Ok(None)
}

fn owner_manifest_for_driver_in(
    tx: &Transaction<'_>,
    driver_path: &str,
) -> Result<Option<String>, StoreError> {
    let Some(relation) = relation_path_for_role(tx, RelationRole::ManifestResource)? else {
        return Ok(None);
    };
    for link in resources_for_manifest_in(tx, LINK_MANIFEST)? {
        let spec: LinkSpec = decode(&link.spec, "Link spec")?;
        if spec.relation == relation && spec.target == driver_path {
            return Ok(Some(spec.source));
        }
    }
    Ok(None)
}

fn refresh_projection(
    tx: &Transaction<'_>,
    resource: &Resource,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    match resource.manifest.as_str() {
        DRIVER_MANIFEST => {
            let spec: DriverSpec = decode(&resource.spec, "Driver spec")?;
            project_driver_manifests(tx, &resource.path, &spec)?;
            reconcile_all_resources(tx, "driver_management_updated", now)?;
        }
        _ => {}
    }
    Ok(())
}

fn driver_path_for_manifest(
    tx: &Transaction<'_>,
    manifest_path: &str,
) -> Result<Option<String>, StoreError> {
    for driver in resources_for_manifest_in(tx, DRIVER_MANIFEST)? {
        let spec: DriverSpec = decode(&driver.spec, "Driver spec")?;
        if spec.manages.iter().any(|managed| managed == manifest_path) {
            return Ok(Some(driver.path.clone()));
        }
    }
    Ok(None)
}

fn driver_for_resource(
    tx: &Transaction<'_>,
    resource: &Resource,
) -> Result<Option<String>, StoreError> {
    driver_path_for_manifest(tx, &resource.manifest)
}

fn matching_drivers(
    tx: &Transaction<'_>,
    resource: &Resource,
) -> Result<std::collections::BTreeMap<String, u64>, StoreError> {
    let mut matched = std::collections::BTreeMap::new();
    if let Some(owner) = driver_for_resource(tx, resource)? {
        let driver = resource_in(tx, &owner)?;
        if driver.metadata.state == "running" {
            matched.insert(owner, driver.revision);
        }
    }
    let drivers = resources_for_manifest_in(tx, DRIVER_MANIFEST)?;
    for driver in drivers {
        if driver.metadata.state != "running" {
            continue;
        }
        let spec: DriverSpec = decode(&driver.spec, "Driver spec")?;
        if spec.watches.iter().any(|watch| watch.matches(resource)) {
            matched.insert(driver.path.clone(), driver.revision);
        }
    }
    Ok(matched)
}

fn enqueue_if_drifted(
    tx: &Transaction<'_>,
    resource: &Resource,
    _reason: &str,
    _now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let drivers = matching_drivers(tx, resource)?;
    let expected = drivers
        .iter()
        .map(|(driver, driver_revision)| {
            (
                driver.clone(),
                DriverObservation {
                    driver_revision: *driver_revision,
                    resource_revision: resource.revision,
                },
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut updated = resource.clone();
    let mut changed = false;
    if expected != resource.metadata.kas.observed {
        updated.metadata.kas.observed = expected.clone();
        changed = true;
    }
    let mut actual = resource.status.metadata.kas.observed.clone();
    actual.retain(|driver, _| expected.contains_key(driver));
    if actual != resource.status.metadata.kas.observed {
        updated.status.metadata.kas.observed = actual;
        changed = true;
    }
    if changed {
        save_resource_in(tx, &updated)?;
    }
    Ok(())
}

fn next_reconciliation_in(
    tx: &Transaction<'_>,
    driver_path: &str,
) -> Result<Option<(String, u64)>, StoreError> {
    let pending = tx
        .query_row(
            "SELECT resources.path,
                    json_extract(expected.value,'$.driver_revision')
             FROM resources
             JOIN json_each(resources.metadata,'$.\"[kas]\".observed') AS expected
             LEFT JOIN json_each(resources.status,'$.metadata.\"[kas]\".observed') AS actual
               ON actual.key=expected.key
             WHERE expected.key=?
               AND (
                 actual.key IS NULL
                 OR json_extract(actual.value,'$.driver_revision')
                    != json_extract(expected.value,'$.driver_revision')
                 OR json_extract(actual.value,'$.resource_revision')
                    != json_extract(expected.value,'$.resource_revision')
               )
             ORDER BY json_extract(resources.metadata,'$.\"[kas]\".created_at'),
                      resources.path
             LIMIT 1",
            [driver_path],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
        )
        .optional()?;
    if pending.is_some() {
        return Ok(pending);
    }

    let driver = resource_in(tx, driver_path)?;
    let spec: DriverSpec = decode(&driver.spec, "Driver spec")?;
    for manifest in spec.manages {
        let drifted = tx
            .query_row(
                "SELECT resources.path,
                        json_extract(expected.value,'$.driver_revision')
                 FROM resources
                 JOIN json_each(resources.metadata,'$.\"[kas]\".observed') AS expected
                 WHERE expected.key=?
                   AND json_extract(resources.metadata,'$.manifest')=?
                   AND (
                     json_remove(resources.metadata,'$.\"[kas]\".observed')
                       IS NOT json_remove(
                         json_extract(resources.status,'$.metadata'),
                         '$.\"[kas]\".observed'
                       )
                     OR json(resources.spec)
                       IS NOT json_quote(json_extract(resources.status,'$.spec'))
                   )
                 ORDER BY json_extract(resources.metadata,'$.\"[kas]\".created_at'),
                          resources.path
                 LIMIT 1",
                params![driver_path, manifest],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
            )
            .optional()?;
        if drifted.is_some() {
            return Ok(drifted);
        }
    }
    Ok(None)
}

fn reconcile_all_resources(
    tx: &Transaction<'_>,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    for resource in all_resources_in(tx)? {
        enqueue_if_drifted(tx, &resource, reason, now)?;
    }
    Ok(())
}

fn reconcile_platform_state(store: &mut Store) -> Result<(), StoreError> {
    let tx = store.connection.transaction()?;
    let now = Utc::now();
    reconcile_all_resources(&tx, "startup_resync", now)?;
    tx.commit()?;
    Ok(())
}

fn apply_mutation(
    tx: &Transaction<'_>,
    driver_path: &str,
    generation: u64,
    operation: Mutation,
    now: DateTime<Utc>,
) -> Result<Value, StoreError> {
    match operation {
        Mutation::CreateResource { resource } => {
            validate_resource_identity(&resource)?;
            if resource.manifest == PACKAGE_MANIFEST {
                return Err(StoreError::Invalid(
                    "Package Resources can only be created by POST /packages".into(),
                ));
            }
            if resource.manifest == CREDENTIAL_MANIFEST {
                return Err(StoreError::Invalid(
                    "Credential Resources can only be created by POST /credentials".into(),
                ));
            }
            let resource = normalized_initial_documents(tx, &resource)?;
            validate_against_manifest(
                tx,
                &resource.manifest,
                &resource.metadata.state,
                &resource.spec,
                &resource.status,
            )?;
            insert_resource_row(tx, &resource, false, "driver", now)?;
            let owner_manifest = owner_manifest_for_driver_in(tx, driver_path)?.unwrap_or_default();
            project_resource(tx, &resource, &owner_manifest, now)?;
            let created = resource_in(tx, &resource.path)?;
            project_declared_relationships(tx, &planned_from_resource(created.clone()), now)?;
            append_event(tx, EventType::Created, &created, now)?;
            enqueue_if_drifted(tx, &created, "driver_created", now)?;
            Ok(serde_json::to_value(created)?)
        }
        Mutation::UpdateResource {
            resource_path,
            expected_revision,
            metadata,
            spec,
        } => {
            let mut current = resource_in(tx, &resource_path)?;
            if current.manifest == CREDENTIAL_MANIFEST {
                return Err(StoreError::Invalid(
                    "Credential Resources can only be changed through credential endpoints".into(),
                ));
            }
            let state = metadata
                .as_ref()
                .map(|metadata| metadata.state.as_str())
                .unwrap_or(&current.metadata.state);
            validate_against_manifest(tx, &current.manifest, state, &spec, &current.status)?;
            if current.revision != expected_revision {
                return Err(StoreError::Conflict("Resource revision is stale".into()));
            }
            current.spec = spec;
            current.metadata.state = state.into();
            current.metadata.kas.revision += 1;
            current.metadata.kas.updated_at = now;
            save_resource_in(tx, &current)?;
            let updated = resource_in(tx, &resource_path)?;
            refresh_projection(tx, &updated, now)?;
            append_event(tx, EventType::Updated, &updated, now)?;
            enqueue_if_drifted(tx, &updated, "driver_spec_updated", now)?;
            Ok(serde_json::to_value(updated)?)
        }
        Mutation::DeleteResource {
            resource_path,
            expected_revision,
        } => {
            let mut resource = resource_in(tx, &resource_path)?;
            if resource.revision != expected_revision {
                return Err(StoreError::Conflict("Resource revision is stale".into()));
            }
            resource.metadata.state = STATE_DELETED.into();
            resource.metadata.kas.revision += 1;
            resource.metadata.kas.updated_at = now;
            save_resource_in(tx, &resource)?;
            resource = resource_in(tx, &resource_path)?;
            append_event(tx, EventType::Updated, &resource, now)?;
            enqueue_if_drifted(tx, &resource, "driver_delete_requested", now)?;
            if driver_for_resource(tx, &resource)?.is_none() {
                let mut status = resource.status.clone();
                let actual = status.metadata.kas.observed.clone();
                status.metadata = resource.metadata.clone();
                status.metadata.kas.observed = actual;
                status.spec = resource.spec.clone();
                resource.status = status;
                save_resource_in(tx, &resource)?;
                maybe_finish_deleted_resource(tx, &resource_path, now)?;
            }
            Ok(serde_json::to_value(resource)?)
        }
        Mutation::UpdateResourceStatus {
            resource_path,
            expected_revision,
            mut status,
        } => {
            assert_driver_owns(tx, &resource_path, driver_path, generation)?;
            let mut current = resource_in(tx, &resource_path)?;
            normalize_submitted_status(&current, &mut status);
            validate_against_manifest(
                tx,
                &current.manifest,
                &current.metadata.state,
                &current.spec,
                &status,
            )?;
            if current.revision != expected_revision {
                return Err(StoreError::Conflict("Resource revision is stale".into()));
            }
            current.status = status;
            save_resource_in(tx, &current)?;
            let updated = resource_in(tx, &resource_path)?;
            refresh_projection(tx, &updated, now)?;
            append_event(tx, EventType::Updated, &updated, now)?;
            maybe_finish_deleted_resource(tx, &resource_path, now)?;
            Ok(serde_json::to_value(updated)?)
        }
        Mutation::CompleteRun { run_path, result } => {
            let mut run = resource_in(tx, &run_path)?;
            require_manifest(&run, RUN_MANIFEST)?;
            let mut spec: RunSpec = decode(&run.spec, "Run spec")?;
            if spec.driver_generation != Some(generation)
                || spec.driver.as_deref() != Some(driver_path)
            {
                return Err(StoreError::Conflict("Run generation is stale".into()));
            }
            let state = apply_run_result(&mut spec, result);
            spec.finished_at = Some(now);
            update_platform_resource(tx, &run_path, state, &spec, now)?;
            run = resource_in(tx, &run_path)?;
            append_event(tx, EventType::Updated, &run, now)?;
            Ok(serde_json::to_value(run)?)
        }
    }
}

fn hard_delete_resource(
    tx: &Transaction<'_>,
    path: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let resource = resource_in(tx, path)?;
    let run_paths = {
        let mut statement = tx.prepare(
            "SELECT path FROM resources
             WHERE json_extract(metadata,'$.manifest')='/builtin/run'
               AND json_extract(spec,'$.resource')=?",
        )?;
        let rows = statement
            .query_map([path], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for run in run_paths {
        hard_delete_resource(tx, &run, now)?;
    }
    append_deleted_event(tx, &resource, now)?;
    tx.execute("DELETE FROM resources WHERE path=?", [path])?;
    Ok(())
}

fn maybe_finish_deleted_resource(
    tx: &Transaction<'_>,
    path: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let resource = resource_in(tx, path)?;
    if resource.metadata.state != STATE_DELETED || resource.status.metadata.state != STATE_DELETED {
        return Ok(());
    }
    let complete = resource
        .metadata
        .kas
        .observed
        .iter()
        .all(|(driver, expected)| {
            resource.status.metadata.kas.observed.get(driver) == Some(expected)
        });
    if complete {
        hard_delete_resource(tx, path, now)?;
    }
    Ok(())
}

fn normalize_submitted_status(resource: &Resource, status: &mut ResourceStatus) {
    status.metadata.manifest = resource.metadata.manifest.clone();
    status.metadata.name = resource.metadata.name.clone();
    status.metadata.kas.revision = resource.metadata.kas.revision;
    status.metadata.kas.generation = resource.metadata.kas.generation;
    status.metadata.kas.protected = resource.metadata.kas.protected;
    status.metadata.kas.managed_by = resource.metadata.kas.managed_by.clone();
    status.metadata.kas.created_at = resource.metadata.kas.created_at;
    status.metadata.kas.updated_at = resource.metadata.kas.updated_at;
    status.metadata.kas.observed = resource.status.metadata.kas.observed.clone();
}

fn assert_driver_owns(
    tx: &Transaction<'_>,
    resource_path: &str,
    driver_path: &str,
    generation: u64,
) -> Result<(), StoreError> {
    assert_driver_generation(tx, driver_path, generation)?;
    let resource = resource_in(tx, resource_path)?;
    let owner = driver_for_resource(tx, &resource)?;
    if owner.as_deref() != Some(driver_path) {
        return Err(StoreError::Conflict("Driver does not own Resource".into()));
    }
    Ok(())
}

fn assert_driver_generation(
    tx: &Transaction<'_>,
    driver_path: &str,
    generation: u64,
) -> Result<(), StoreError> {
    let driver = resource_in(tx, driver_path)?;
    require_manifest(&driver, DRIVER_MANIFEST)?;
    if driver.metadata.kas.generation != generation
        || driver_state(&driver)? != DriverState::Running
    {
        return Err(StoreError::Conflict("Driver generation is stale".into()));
    }
    Ok(())
}

fn update_status_document<S: Serialize, T: Serialize>(
    tx: &Transaction<'_>,
    path: &str,
    state: S,
    status: &T,
    _now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let state = serde_json::to_value(state)?;
    let state = state
        .as_str()
        .ok_or_else(|| StoreError::Invalid("Resource status state must be a string".into()))?;
    let mut resource = resource_in(tx, path)?;
    let mut metadata = resource.status_metadata(state);
    metadata.kas.observed = resource.status.metadata.kas.observed.clone();
    let status = ResourceStatus {
        metadata,
        spec: serde_json::to_value(status)?,
    };
    resource.status = status;
    save_resource_in(tx, &resource)?;
    Ok(())
}

fn update_platform_resource<S: Serialize, T: Serialize>(
    tx: &Transaction<'_>,
    path: &str,
    state: S,
    spec: &T,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let state = serde_json::to_value(state)?;
    let state = state
        .as_str()
        .ok_or_else(|| StoreError::Invalid("Resource state must be a string".into()))?;
    let mut current = resource_in(tx, path)?;
    let spec = serde_json::to_value(spec)?;
    let mut metadata = current.metadata.clone();
    metadata.state = state.into();
    metadata.kas.revision += 1;
    metadata.kas.updated_at = now;
    let mut status_metadata = metadata.clone();
    status_metadata.kas.observed = current.status.metadata.kas.observed;
    let status = ResourceStatus {
        metadata: status_metadata,
        spec: spec.clone(),
    };
    current.metadata = metadata;
    current.spec = spec;
    current.status = status;
    save_resource_in(tx, &current)?;
    let updated = resource_in(tx, path)?;
    enqueue_if_drifted(tx, &updated, "platform_updated", now)?;
    Ok(())
}

fn apply_run_result(spec: &mut RunSpec, result: RunResult) -> RunState {
    match result {
        RunResult::Succeeded { output } => {
            spec.output = Some(output);
            spec.error = None;
            RunState::Succeeded
        }
        RunResult::Failed { error } => {
            spec.output = None;
            spec.error = Some(error);
            RunState::Failed
        }
    }
}

fn driver_state(resource: &Resource) -> Result<DriverState, StoreError> {
    decode(
        &Value::String(resource.status.metadata.state.clone()),
        "Driver status state",
    )
}

fn run_state(resource: &Resource) -> Result<RunState, StoreError> {
    decode(
        &Value::String(resource.status.metadata.state.clone()),
        "Run status state",
    )
}

fn run_state_name(state: RunState) -> &'static str {
    match state {
        RunState::Queued => "queued",
        RunState::Running => "running",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
    }
}

fn append_event(
    tx: &Transaction<'_>,
    event_type: EventType,
    resource: &Resource,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO events(event_type,resource_path,revision,value_json,created_at)
         VALUES (?,?,?,?,?)",
        params![
            event_type_name(event_type),
            resource.path,
            resource.revision,
            serde_json::to_string(resource)?,
            stamp(now)
        ],
    )?;
    Ok(())
}

fn append_deleted_event(
    tx: &Transaction<'_>,
    resource: &Resource,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO events(event_type,resource_path,revision,value_json,created_at)
         VALUES ('deleted',?,?,?,?)",
        params![
            resource.path,
            resource.revision,
            serde_json::to_string(resource)?,
            stamp(now)
        ],
    )?;
    Ok(())
}

fn event_type_name(event_type: EventType) -> &'static str {
    match event_type {
        EventType::Created => "created",
        EventType::Updated => "updated",
        EventType::Deleted => "deleted",
    }
}

fn all_resources_in(tx: &Transaction<'_>) -> Result<Vec<Resource>, StoreError> {
    let mut statement = tx.prepare(&format!(
        "{RESOURCE_SELECT}
         ORDER BY json_extract(metadata,'$.\"[kas]\".created_at'),path"
    ))?;
    let rows = statement.query_map([], resource_from_row)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn resources_for_manifest_in(
    tx: &Transaction<'_>,
    manifest: &str,
) -> Result<Vec<Resource>, StoreError> {
    let mut statement = tx.prepare(&format!(
        "{RESOURCE_SELECT}
         WHERE json_extract(metadata,'$.manifest')=?
         ORDER BY json_extract(metadata,'$.\"[kas]\".created_at'),path"
    ))?;
    let rows = statement.query_map([manifest], resource_from_row)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn save_resource_in(tx: &Transaction<'_>, resource: &Resource) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE resources SET metadata=?,spec=?,status=? WHERE path=?",
        params![
            serde_json::to_string(&resource.metadata)?,
            serde_json::to_string(&resource.spec)?,
            serde_json::to_string(&resource.status)?,
            resource.path
        ],
    )?;
    Ok(())
}

fn require_manifest(resource: &Resource, expected: &str) -> Result<(), StoreError> {
    if resource.manifest != expected {
        return Err(StoreError::Invalid(format!(
            "Resource {} must use Manifest {expected}",
            resource.path
        )));
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(value: &Value, kind: &str) -> Result<T, StoreError> {
    serde_json::from_value(value.clone())
        .map_err(|error| StoreError::Invalid(format!("invalid {kind}: {error}")))
}

fn json_from_row<T: DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let raw: String = row.get(index)?;
    serde_json::from_str(&raw).map_err(|error| from_sql(index, error))
}

fn time_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<DateTime<Utc>> {
    let raw: String = row.get(index)?;
    DateTime::parse_from_rfc3339(&raw)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| from_sql(index, error))
}

fn stamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn from_sql(
    index: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(error))
}

fn constraint(error: rusqlite::Error, message: &str) -> StoreError {
    match error {
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::ConstraintViolation | rusqlite::ErrorCode::DatabaseBusy
            ) =>
        {
            StoreError::Conflict(message.into())
        }
        other => StoreError::Database(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kas_core::{
        ManifestDefinition, PackageDefinition, PlannedResourceMetadata, ResourceDefinition,
    };

    fn planned(path: &str, manifest: &str, name: &str, spec: Value) -> PlannedResource {
        PlannedResource {
            path: path.into(),
            metadata: PlannedResourceMetadata {
                manifest: manifest.into(),
                name: name.into(),
                state: String::new(),
            },
            spec,
            status: ResourceStatus::default(),
        }
    }

    fn echo_manifest() -> PackageExpansion {
        PackageDefinition {
            manifest: ManifestDefinition {
                path: "/manifests/test/echo".into(),
                manifest: MANIFEST_MANIFEST.into(),
                name: "echo".into(),
                version: 1,
                description: "Store test Manifest".into(),
                resource_schema: json!({
                    "type": "object",
                    "properties": {"label": {"type": "string"}},
                    "required": ["label"],
                    "additionalProperties": false
                }),
                states: vec![],
                default_state: STATE_AVAILABLE.into(),
                initial_state: kas_core::STATE_PENDING.into(),
            },
            resources: vec![ResourceDefinition {
                path: "./relations/peer".into(),
                metadata: PlannedResourceMetadata {
                    manifest: RELATION_MANIFEST.into(),
                    name: "peer".into(),
                    state: String::new(),
                },
                spec: json!({
                    "sources": [{"manifest": "/manifests/test/echo"}],
                    "targets": [{"manifest": "/manifests/test/echo"}],
                    "on_source_delete": "unlink",
                    "metadata_schema": {"type": "object"}
                }),
                status: ResourceStatus::default(),
            }],
        }
        .expand(format!("sha256:{}", "deadbeef".repeat(8)))
        .unwrap()
    }

    #[test]
    fn bootstraps_the_self_describing_resource_universe() {
        let store = Store::memory().unwrap();

        let root = store.get_resource(MANIFEST_MANIFEST).unwrap();
        assert_eq!(root.path, root.manifest);
        assert_eq!(root.metadata.state, STATE_AVAILABLE);
        assert!(store
            .list_resources(Some(MANIFEST_MANIFEST))
            .unwrap()
            .iter()
            .any(|resource| resource.path == LINK_MANIFEST));
        let package = store.package_for_manifest(MANIFEST_MANIFEST).unwrap();
        assert_eq!(package.manifest, PACKAGE_MANIFEST);
        assert!(decode::<PackageSpec>(&package.spec, "Package spec")
            .unwrap()
            .digest
            .starts_with("sha256:"));
        let mut builtin_packages = std::collections::BTreeSet::from([package.path.clone()]);
        for manifest_path in [
            ACTION_MANIFEST,
            RELATION_MANIFEST,
            LINK_MANIFEST,
            DRIVER_MANIFEST,
            RUN_MANIFEST,
            USER_MANIFEST,
            SERVICE_ACCOUNT_MANIFEST,
            ROLE_MANIFEST,
            CREDENTIAL_MANIFEST,
            PACKAGE_MANIFEST,
        ] {
            builtin_packages.insert(
                store
                    .package_for_manifest(manifest_path)
                    .unwrap()
                    .path
                    .clone(),
            );
        }
        assert_eq!(builtin_packages.len(), 11);
        assert!(store
            .list_resources(Some(ROLE_MANIFEST))
            .unwrap()
            .iter()
            .any(|resource| {
                decode::<RoleSpec>(&resource.spec, "Role spec")
                    .is_ok_and(|role| role.system_role == Some(SystemRole::Admin))
            }));
        let relation_driver = store
            .driver_for_manifest(RELATION_MANIFEST)
            .unwrap()
            .unwrap();
        let link_driver = store.driver_for_manifest(LINK_MANIFEST).unwrap().unwrap();
        assert_eq!(relation_driver.path, "/builtin/link/driver");
        assert_eq!(relation_driver.path, link_driver.path);
        assert!(matches!(
            store.get_resource("/builtin/relation/driver"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn generic_resource_creation_applies_manifest_states_and_schema() {
        let mut store = Store::memory().unwrap();
        store
            .install_package(echo_manifest(), 123, kas_core::MANIFEST_PACKAGE_MEDIA_TYPE)
            .unwrap();

        let created = store
            .create_resource(planned(
                "/resources/test/echo-1",
                "/manifests/test/echo",
                "echo-1",
                json!({"label": "one"}),
            ))
            .unwrap();
        assert_eq!(created.spec, json!({"label": "one"}));
        assert_eq!(created.status.spec, created.spec);
        assert_eq!(created.metadata.state, STATE_AVAILABLE);
        assert_eq!(created.status.metadata.state, kas_core::STATE_PENDING);
        assert_eq!(created.status.metadata.manifest, created.metadata.manifest);
        assert_eq!(created.status.metadata.name, created.metadata.name);
        assert_eq!(
            created.status.metadata.kas.revision,
            created.metadata.kas.revision
        );
        assert!(created.metadata.kas.observed.is_empty());
        assert!(created.status.metadata.kas.observed.is_empty());
        assert_eq!(
            store
                .list_events(None, 100)
                .unwrap()
                .last()
                .unwrap()
                .resource_path,
            created.path
        );

        let invalid = store.create_resource(planned(
            "/resources/test/invalid",
            "/manifests/test/echo",
            "invalid",
            json!({"label": 7}),
        ));
        assert!(matches!(invalid, Err(StoreError::Invalid(_))));

        let forged_package = store.create_resource(planned(
            "/packages/sha256/cafe",
            PACKAGE_MANIFEST,
            "sha256:cafe",
            json!({
                "digest": "sha256:cafe",
                "size_bytes": 1,
                "media_type": kas_core::MANIFEST_PACKAGE_MEDIA_TYPE
            }),
        ));
        assert!(matches!(forged_package, Err(StoreError::Invalid(_))));

        let mut unknown = planned(
            "/resources/test/unknown-state",
            "/manifests/test/echo",
            "unknown-state",
            json!({"label": "invalid"}),
        );
        unknown.metadata.state = "mystery".into();
        let unknown_state = store.create_resource(unknown);
        assert!(matches!(unknown_state, Err(StoreError::Invalid(_))));
    }

    #[test]
    fn manifest_cannot_redeclare_platform_states() {
        let mut store = Store::memory().unwrap();
        let mut package = echo_manifest();
        package.resources[0].spec["states"] = json!([STATE_AVAILABLE]);

        let result = store.install_package(package, 123, kas_core::MANIFEST_PACKAGE_MEDIA_TYPE);
        assert!(matches!(result, Err(StoreError::Invalid(_))));
    }

    #[test]
    fn bracketed_business_fields_are_reserved_for_kas() {
        assert!(matches!(
            validate_no_reserved_keys(
                "Resource spec",
                &json!({"nested": {"[kas]": {"forged": true}}})
            ),
            Err(StoreError::Invalid(_))
        ));

        let mut store = Store::memory().unwrap();
        let mut package = echo_manifest();
        package.resources[0].spec["resource_schema"]["properties"]["bad[field]"] =
            json!({"type": "string"});
        let result = store.install_package(package, 123, kas_core::MANIFEST_PACKAGE_MEDIA_TYPE);
        assert!(matches!(result, Err(StoreError::Invalid(_))));
    }

    #[test]
    fn links_are_ordinary_resources_pending_driver_validation() {
        let mut store = Store::memory().unwrap();
        store
            .install_package(echo_manifest(), 123, kas_core::MANIFEST_PACKAGE_MEDIA_TYPE)
            .unwrap();
        let source = store
            .create_resource(planned(
                "/resources/test/echo-1",
                "/manifests/test/echo",
                "echo-1",
                json!({"label": "one"}),
            ))
            .unwrap();
        let target = store
            .create_resource(planned(
                "/resources/test/echo-2",
                "/manifests/test/echo",
                "echo-2",
                json!({"label": "two"}),
            ))
            .unwrap();
        let link = store
            .create_resource(planned(
                "/links/test/peer",
                LINK_MANIFEST,
                "peer",
                json!({
                    "relation": "/manifests/test/echo/relations/peer",
                    "source": source.path.as_str(),
                    "target": target.path.as_str(),
                    "metadata": {}
                }),
            ))
            .unwrap();

        assert_eq!(link.manifest, LINK_MANIFEST);
        assert_eq!(store.links_for_resource(&source.path).unwrap().len(), 1);
        assert_eq!(link.metadata.state, STATE_AVAILABLE);
        assert_eq!(link.status.metadata.state, kas_core::STATE_PENDING);

        let unresolved = store
            .create_resource(planned(
                "/links/test/unresolved",
                LINK_MANIFEST,
                "unresolved",
                json!({
                    "relation": "/manifests/test/echo/relations/peer",
                    "source": "/resources/test/missing",
                    "target": target.path.as_str(),
                    "metadata": {}
                }),
            ))
            .unwrap();
        assert_eq!(unresolved.status.metadata.state, kas_core::STATE_PENDING);
    }

    #[test]
    fn completed_driver_deliveries_are_not_retained() {
        let mut store = Store::memory().unwrap();
        let driver_path = "/builtin/link/driver";
        store.start_driver(driver_path).unwrap();
        let generation = store.driver_generation(driver_path).unwrap();
        store
            .mark_driver_ready(
                driver_path,
                DriverReady {
                    generation,
                    process_id: 1,
                    metadata: json!({}),
                },
            )
            .unwrap();

        let delivery = store
            .claim_driver_delivery(driver_path, generation)
            .unwrap()
            .expect("running Driver has reconciliation work");
        store
            .acknowledge_driver_delivery(delivery.id, driver_path, generation)
            .unwrap();
        store
            .finish_reconciliation_with_mutations(delivery.id, driver_path, generation, Vec::new())
            .unwrap();

        assert!(matches!(
            store.get_driver_delivery(delivery.id),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn unfinished_work_is_rederived_after_store_restart() {
        let database = std::env::temp_dir().join(format!("kas-store-{}.db", Uuid::new_v4()));
        migrate(&database).unwrap();
        let driver_path = "/builtin/link/driver";
        let first_delivery_id;
        let generation;
        {
            let mut store = Store::open(&database).unwrap();
            store.start_driver(driver_path).unwrap();
            generation = store.driver_generation(driver_path).unwrap();
            store
                .mark_driver_ready(
                    driver_path,
                    DriverReady {
                        generation,
                        process_id: 1,
                        metadata: json!({}),
                    },
                )
                .unwrap();
            first_delivery_id = store
                .claim_driver_delivery(driver_path, generation)
                .unwrap()
                .expect("running Driver has reconciliation work")
                .id;
        }

        let mut reopened = Store::open(&database).unwrap();
        let rederived = reopened
            .claim_driver_delivery(driver_path, generation)
            .unwrap()
            .expect("unfinished work is derived from Resource observations");
        assert_ne!(rederived.id, first_delivery_id);
        std::fs::remove_file(database).unwrap();
    }

    #[test]
    fn resources_and_events_are_the_only_persistent_tables() {
        let store = Store::memory().unwrap();
        let mut statement = store
            .connection
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type='table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .unwrap();
        let tables = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(tables, vec!["events", "resources"]);
        let mut statement = store
            .connection
            .prepare("SELECT name FROM pragma_table_info('resources') ORDER BY cid")
            .unwrap();
        let columns = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(columns, vec!["path", "metadata", "spec", "status"]);
    }
}
