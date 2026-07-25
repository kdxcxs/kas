//! SQLite persistence for KAS's single-Resource model.

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use kas_auth::{issue_token, token_hash, AuthContext, IssuedCredential, Rule, Subject};
use kas_core::{
    package_path_for_digest, ActionSpec, CreateResource, CreateRun, CredentialSpec, DeliveryStatus,
    DriverDelivery, DriverDesiredState, DriverReady, DriverSpec, DriverState, DriverStatus,
    DriverWork, Event, EventFilter, EventType, FinishRun, LinkSpec, ManifestDefinition,
    ManifestSpec, Mutation, PackageDefinition, PackageExpansion, PackageSpec, PlannedResource,
    RelationRole, RelationSpec, Resource, ResourceDefinition, RestartPolicy, RoleBindingSpec,
    RoleSpec, RunResult, RunSpec, RunState, RunStatus, SystemRole, UpdateResource,
    UpdateResourceStatus, UserSpec, BUILTIN_PACKAGE_MEDIA_TYPE, STATE_AVAILABLE, STATE_DELETED,
};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const LATEST_SCHEMA_VERSION: u32 = 9;

pub const MANIFEST_MANIFEST: &str = "/builtin/manifest";
pub const ACTION_MANIFEST: &str = "/builtin/action";
pub const RELATION_MANIFEST: &str = "/builtin/relation";
pub const LINK_MANIFEST: &str = "/builtin/link";
pub const DRIVER_MANIFEST: &str = "/builtin/driver";
pub const RUN_MANIFEST: &str = "/builtin/run";
pub const USER_MANIFEST: &str = "/builtin/user";
pub const SERVICE_ACCOUNT_MANIFEST: &str = "/builtin/service-account";
pub const ROLE_MANIFEST: &str = "/builtin/role";
pub const ROLE_BINDING_MANIFEST: &str = "/builtin/role-binding";
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
];

const RESOURCE_SELECT: &str =
    "SELECT path,manifest_path,name,spec_json,status_json,revision,created_at,updated_at
     FROM resources";

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
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        configure(&connection)?;
        require_current_schema(&connection)?;
        let mut store = Self { connection };
        store.ensure_builtins()?;
        reconcile_platform_state(&mut store)?;
        Ok(store)
    }

    pub fn memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        configure(&connection)?;
        let mut store = Self { connection };
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
                if package.path != package_path {
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
                    let (planned, status) = normalized_initial_documents(&tx, planned)?;
                    insert_resource_row(
                        &tx,
                        &planned,
                        &status,
                        true,
                        &format!("package:{}", root.path),
                        now,
                    )?;
                    stored_paths.push(planned.path.clone());
                    projections.push((planned, status, owner_manifest.clone()));
                }
            }
        }
        projections.sort_by_key(|(planned, _, _)| match planned.manifest.as_str() {
            MANIFEST_MANIFEST => 0,
            LINK_MANIFEST => 2,
            RUN_MANIFEST => 3,
            _ => 1,
        });
        for (planned, status, owner_manifest) in &projections {
            project_resource(&tx, planned, status, owner_manifest, now)?;
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
                manifest: PACKAGE_MANIFEST.into(),
                name: package_spec.digest.clone(),
                spec: serde_json::to_value(&package_spec)?,
                status: serde_json::to_value(&package_spec)?,
            };
            validate_resource_identity(&package)?;
            let (package, package_status) = normalized_initial_documents(&tx, &package)?;
            insert_resource_row(
                &tx,
                &package,
                &package_status,
                true,
                &format!("package:{}", root.path),
                now,
            )?;
            project_resource(&tx, &package, &package_status, &root.path, now)?;
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
                    Some(&package.path),
                    Some(&root.path),
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
                        Some(owner),
                        Some(&packaged_resource.path),
                        now,
                    )?;
                }
            }
        }
        for resource in &stored_resources {
            project_declared_relationships(&tx, resource, now)?;
        }
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
        let tx = self.connection.transaction()?;
        let now = Utc::now();
        let (input, status) = normalized_initial_documents(&tx, &input)?;
        validate_against_manifest(&tx, &input.manifest, &input.spec, &status)?;
        insert_resource_row(&tx, &input, &status, false, "user", now)?;
        project_resource(&tx, &input, &status, "", now)?;
        let resource = tx.query_row(
            &format!("{RESOURCE_SELECT} WHERE path=?"),
            [&input.path],
            resource_from_row,
        )?;
        project_declared_relationships(&tx, &planned_from_resource(resource.clone()), now)?;
        append_event(&tx, EventType::Created, &resource, now)?;
        enqueue_if_drifted(&tx, &resource, "created", now)?;
        reconcile_ensures(&tx, now)?;
        tx.commit()?;
        Ok(resource)
    }

    pub fn list_resources(&self, manifest: Option<&str>) -> Result<Vec<Resource>, StoreError> {
        let sql = if manifest.is_some() {
            format!("{RESOURCE_SELECT} WHERE manifest_path=? ORDER BY created_at,path")
        } else {
            format!("{RESOURCE_SELECT} ORDER BY created_at,path")
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
        let current = resource_in(&tx, path)?;
        validate_against_manifest(&tx, &current.manifest, &input.spec, &current.status)?;
        let now = Utc::now();
        let changed = tx.execute(
            "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=?
             WHERE path=? AND revision=? AND protected=0",
            params![
                serde_json::to_string(&input.spec)?,
                stamp(now),
                path,
                input.expected_revision
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(format!(
                "Resource {path} revision is stale or protected"
            )));
        }
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
        input: UpdateResourceStatus,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        assert_driver_owns(&tx, path, driver_path, generation)?;
        let current = resource_in(&tx, path)?;
        validate_against_manifest(&tx, &current.manifest, &current.spec, &input.status)?;
        let now = Utc::now();
        let changed = tx.execute(
            "UPDATE resources SET status_json=?,updated_at=?
             WHERE path=? AND revision=?",
            params![
                serde_json::to_string(&input.status)?,
                stamp(now),
                path,
                input.expected_revision
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict("Resource revision is stale".into()));
        }
        let resource = resource_in(&tx, path)?;
        refresh_projection(&tx, &resource, now)?;
        append_event(&tx, EventType::Updated, &resource, now)?;
        if resource.spec == resource.status && document_state(&resource.spec) == Some(STATE_DELETED)
        {
            hard_delete_resource(&tx, path, now)?;
        } else {
            enqueue_if_drifted(&tx, &resource, "status_updated", now)?;
        }
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
        if is_protected(&tx, path)? {
            return Err(StoreError::Conflict(format!(
                "Resource {path} is protected"
            )));
        }
        set_document_state(&mut resource.spec, STATE_DELETED)?;
        let now = Utc::now();
        tx.execute(
            "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=? WHERE path=?",
            params![serde_json::to_string(&resource.spec)?, stamp(now), path],
        )?;
        resource = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &resource, now)?;
        if driver_for_resource(&tx, &resource)?.is_some() {
            enqueue_if_drifted(&tx, &resource, "delete_requested", now)?;
        } else {
            tx.execute(
                "UPDATE resources SET status_json=?,updated_at=? WHERE path=?",
                params![serde_json::to_string(&resource.spec)?, stamp(now), path],
            )?;
            hard_delete_resource(&tx, path, now)?;
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
        let mut statement = self.connection.prepare(
            "SELECT r.path,r.manifest_path,r.name,r.spec_json,r.status_json,r.revision,
                    r.created_at,r.updated_at
             FROM link_index l JOIN resources r ON r.path=l.link_path
             WHERE (?1 IS NULL OR l.source_path=?1 OR (?4=1 AND l.target_path=?1))
               AND (?2 IS NULL OR l.relation_path=?2)
               AND (?3 IS NULL OR l.target_path=?3 OR (?4=1 AND l.source_path=?3))
             ORDER BY r.created_at,r.path",
        )?;
        let rows = statement.query_map(
            params![source, relation, target, either_endpoint],
            resource_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_link(&self, path: &str) -> Result<Resource, StoreError> {
        let resource = self.get_resource(path)?;
        require_manifest(&resource, LINK_MANIFEST)?;
        Ok(resource)
    }

    pub fn delete_link(&mut self, path: &str) -> Result<(), StoreError> {
        let resource = self.get_link(path)?;
        if is_protected(&self.connection, path)? {
            return Err(StoreError::Conflict(format!("Link {path} is protected")));
        }
        let tx = self.connection.transaction()?;
        hard_delete_resource(&tx, &resource.path, Utc::now())?;
        tx.commit()?;
        Ok(())
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
        let path: Option<String> = self
            .connection
            .query_row(
                "SELECT driver_path FROM driver_runtime WHERE owner_manifest_path=?",
                [manifest_path],
                |row| row.get(0),
            )
            .optional()?;
        path.map(|path| self.get_resource(&path)).transpose()
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
        let mut status: DriverStatus = decode(&driver.status, "Driver status")?;
        if !matches!(status.state, DriverState::Stopped | DriverState::Failed) {
            return Err(StoreError::Conflict(format!(
                "Driver {path} cannot start from {:?}",
                status.state
            )));
        }
        status.desired_state = DriverDesiredState::Running;
        status.state = DriverState::Starting;
        status.generation += 1;
        status.process_id = None;
        status.error = None;
        let now = Utc::now();
        update_status_document(&tx, path, &status, now)?;
        tx.execute(
            "UPDATE driver_runtime SET generation=?,process_id=NULL,started_at=?,
             heartbeat_at=NULL,stopped_at=NULL,error=NULL WHERE driver_path=?",
            params![status.generation, stamp(now), path],
        )?;
        driver = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &driver, now)?;
        tx.commit()?;
        Ok(driver)
    }

    pub fn mark_driver_ready(
        &mut self,
        path: &str,
        ready: DriverReady,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut driver = resource_in(&tx, path)?;
        let mut status: DriverStatus = decode(&driver.status, "Driver status")?;
        if status.generation != ready.generation || status.state != DriverState::Starting {
            return Err(StoreError::Conflict("Driver generation is stale".into()));
        }
        status.state = DriverState::Ready;
        status.process_id = Some(ready.process_id);
        status.metadata = ready.metadata;
        status.error = None;
        let now = Utc::now();
        update_status_document(&tx, path, &status, now)?;
        tx.execute(
            "UPDATE driver_runtime SET process_id=?,heartbeat_at=? WHERE driver_path=?",
            params![ready.process_id, stamp(now), path],
        )?;
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
        let status: DriverStatus = decode(&driver.status, "Driver status")?;
        if status.generation != generation || status.state != DriverState::Ready {
            return Err(StoreError::Conflict("Driver generation is stale".into()));
        }
        self.connection.execute(
            "UPDATE driver_runtime SET heartbeat_at=? WHERE driver_path=?",
            params![stamp(Utc::now()), path],
        )?;
        Ok(driver)
    }

    pub fn stop_driver(&mut self, path: &str) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut driver = resource_in(&tx, path)?;
        let mut status: DriverStatus = decode(&driver.status, "Driver status")?;
        status.desired_state = DriverDesiredState::Stopped;
        if matches!(status.state, DriverState::Starting | DriverState::Ready) {
            status.state = DriverState::Stopping;
        }
        let now = Utc::now();
        update_status_document(&tx, path, &status, now)?;
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
        let mut status: DriverStatus = decode(&driver.status, "Driver status")?;
        if status.generation != generation {
            return Err(StoreError::Conflict("Driver generation is stale".into()));
        }
        status.state = DriverState::Failed;
        status.process_id = None;
        status.error = Some(error.into());
        let now = Utc::now();
        update_status_document(&tx, path, &status, now)?;
        tx.execute(
            "UPDATE driver_runtime SET process_id=NULL,error=?,stopped_at=? WHERE driver_path=?",
            params![status.error, stamp(now), path],
        )?;
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
        let mut status: DriverStatus = decode(&driver.status, "Driver status")?;
        if status.generation != generation {
            return Err(StoreError::Conflict("Driver generation is stale".into()));
        }
        status.desired_state = DriverDesiredState::Stopped;
        status.state = DriverState::Stopped;
        status.process_id = None;
        status.error = None;
        let now = Utc::now();
        update_status_document(&tx, path, &status, now)?;
        tx.execute(
            "UPDATE driver_runtime SET process_id=NULL,stopped_at=?,error=NULL WHERE driver_path=?",
            params![stamp(now), path],
        )?;
        driver = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &driver, now)?;
        tx.commit()?;
        Ok(driver)
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
            driver: Some(driver.path),
            input: input.input,
        })?;
        let status = serde_json::to_value(RunStatus {
            state: RunState::Queued,
            driver_generation: None,
            output: None,
            error: None,
        })?;
        self.create_resource(PlannedResource {
            path: input.path,
            manifest: RUN_MANIFEST.into(),
            name: input.request_id.to_string(),
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
        let mut status: RunStatus = decode(&run.status, "Run status")?;
        if status.driver_generation != Some(input.driver_generation)
            || status.state != RunState::Running
        {
            return Err(StoreError::Conflict("Run generation is stale".into()));
        }
        apply_run_result(&mut status, input.result);
        let now = Utc::now();
        update_status_document(&tx, run_path, &status, now)?;
        tx.execute(
            "UPDATE run_runtime SET finished_at=? WHERE run_path=?",
            params![stamp(now), run_path],
        )?;
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
        let tx = self.connection.transaction()?;
        let delivery = delivery_in(&tx, delivery_id)?;
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
        tx.execute(
            "UPDATE driver_deliveries SET status='completed',completed_at=? WHERE id=?",
            params![stamp(now), delivery_id.to_string()],
        )?;
        match delivery.work {
            DriverWork::Reconcile { resource } => {
                tx.execute(
                    "DELETE FROM reconcile_queue WHERE resource_path=?",
                    [&resource.path],
                )?;
                if let Ok(current) = resource_in(&tx, &resource.path) {
                    enqueue_if_drifted(&tx, &current, "delivery_completed", now)?;
                }
            }
            DriverWork::Run { .. } => {}
        }
        tx.commit()?;
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

        if let Some(delivery) = tx
            .query_row(
                &format!(
                    "{DELIVERY_SELECT} WHERE driver_path=? AND generation=?
                     AND status IN ('pending','acked') ORDER BY created_at,id LIMIT 1"
                ),
                params![driver_path, generation],
                delivery_from_row,
            )
            .optional()?
        {
            tx.commit()?;
            return Ok(Some(delivery));
        }

        let now = Utc::now();
        let reconcile_path: Option<String> = tx
            .query_row(
                "SELECT resource_path FROM reconcile_queue
                 WHERE driver_path=? AND available_at<=?
                 ORDER BY available_at,updated_at,resource_path LIMIT 1",
                params![driver_path, stamp(now)],
                |row| row.get(0),
            )
            .optional()?;
        let work = if let Some(path) = reconcile_path {
            Some(DriverWork::Reconcile {
                resource: resource_in(&tx, &path)?,
            })
        } else {
            let run_path: Option<String> = tx
                .query_row(
                    "SELECT rr.run_path FROM run_runtime rr
                     JOIN resources r ON r.path=rr.run_path
                     WHERE rr.driver_path=? AND json_extract(r.status_json,'$.state')='queued'
                     ORDER BY r.created_at,r.path LIMIT 1",
                    [driver_path],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(path) = run_path {
                let mut run = resource_in(&tx, &path)?;
                let spec: RunSpec = decode(&run.spec, "Run spec")?;
                let mut status: RunStatus = decode(&run.status, "Run status")?;
                status.state = RunState::Running;
                status.driver_generation = Some(generation);
                update_status_document(&tx, &path, &status, now)?;
                tx.execute(
                    "UPDATE run_runtime SET driver_generation=?,started_at=? WHERE run_path=?",
                    params![generation, stamp(now), path],
                )?;
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
        tx.execute(
            "INSERT INTO driver_deliveries(id,driver_path,generation,work_json,status,created_at)
             VALUES (?,?,?,?,'pending',?)",
            params![
                delivery.id.to_string(),
                driver_path,
                generation,
                serde_json::to_string(&delivery.work)?,
                stamp(now)
            ],
        )?;
        tx.commit()?;
        Ok(Some(delivery))
    }

    pub fn acknowledge_driver_delivery(
        &mut self,
        id: Uuid,
        driver_path: &str,
        generation: u64,
    ) -> Result<DriverDelivery, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE driver_deliveries SET status='acked',acked_at=COALESCE(acked_at,?)
             WHERE id=? AND driver_path=? AND generation=? AND status IN ('pending','acked')",
            params![stamp(now), id.to_string(), driver_path, generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict("Driver delivery is stale".into()));
        }
        self.get_driver_delivery(id)
    }

    pub fn complete_driver_delivery(
        &mut self,
        id: Uuid,
        driver_path: &str,
        generation: u64,
    ) -> Result<DriverDelivery, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE driver_deliveries SET status='completed',completed_at=?
             WHERE id=? AND driver_path=? AND generation=? AND status='acked'",
            params![stamp(now), id.to_string(), driver_path, generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver delivery is not acknowledged".into(),
            ));
        }
        self.get_driver_delivery(id)
    }

    pub fn get_driver_delivery(&self, id: Uuid) -> Result<DriverDelivery, StoreError> {
        self.connection
            .query_row(
                &format!("{DELIVERY_SELECT} WHERE id=?"),
                [id.to_string()],
                delivery_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Driver delivery {id}")))
    }

    pub fn pending_driver_deliveries(
        &self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Vec<DriverDelivery>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DELIVERY_SELECT} WHERE driver_path=? AND generation=?
             AND status IN ('pending','acked') ORDER BY created_at,id"
        ))?;
        let rows = statement.query_map(params![driver_path, generation], delivery_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn manifest_for_driver(&self, driver_path: &str) -> Result<Resource, StoreError> {
        let manifest_path: String = self
            .connection
            .query_row(
                "SELECT owner_manifest_path FROM driver_runtime WHERE driver_path=?",
                [driver_path],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Driver {driver_path}")))?;
        self.get_resource(&manifest_path)
    }

    pub fn package_for_manifest(&self, manifest_path: &str) -> Result<Resource, StoreError> {
        self.connection
            .query_row(
                &format!(
                    "{RESOURCE_SELECT} r
                     JOIN link_index l ON l.source_path=r.path
                     JOIN relation_index relation ON relation.relation_path=l.relation_path
                     WHERE relation.role='package_manifest' AND l.target_path=?"
                ),
                [manifest_path],
                resource_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Package for Manifest {manifest_path}")))
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
                manifest: USER_MANIFEST.into(),
                name: name.into(),
                spec: serde_json::to_value(UserSpec { disabled: false })?,
                status: serde_json::to_value(UserSpec { disabled: false })?,
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
        let binding_path = format!("{path}/role-bindings/admin");
        if self.get_resource(&binding_path).is_err() {
            self.create_resource(PlannedResource {
                path: binding_path,
                manifest: ROLE_BINDING_MANIFEST.into(),
                name: format!("{name}-admin"),
                spec: serde_json::to_value(RoleBindingSpec {
                    role: admin_role.path,
                    subjects: vec![path.clone()],
                })?,
                status: json!({"state": STATE_AVAILABLE}),
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
        let status: DriverStatus = decode(&driver.status, "Driver status")?;
        self.issue_credential_for_generation(
            &spec.service_account,
            Some(Utc::now() + Duration::hours(1)),
            Some(status.generation),
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
            expires_at,
        };
        let tx = self.connection.transaction()?;
        let now = Utc::now();
        let planned = PlannedResource {
            path: credential_path.clone(),
            manifest: CREDENTIAL_MANIFEST.into(),
            name: id.to_string(),
            spec: serde_json::to_value(&spec)?,
            status: json!({"revoked_at": null}),
        };
        let (planned, status) = normalized_initial_documents(&tx, &planned)?;
        insert_resource_row(&tx, &planned, &status, false, "system", now)?;
        tx.execute(
            "INSERT INTO credential_material(credential_path,token_hash,driver_generation)
             VALUES (?,?,?)",
            params![credential_path, token_hash(&token), driver_generation],
        )?;
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
        let (credential_path, driver_generation): (String, Option<u64>) = self
            .connection
            .query_row(
                "SELECT credential_path,driver_generation FROM credential_material
                 WHERE token_hash=?",
                [&hash],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Credential".into()))?;
        let credential = self.get_resource(&credential_path)?;
        let spec: CredentialSpec = decode(&credential.spec, "Credential spec")?;
        if spec.expires_at.is_some_and(|expiry| expiry <= Utc::now())
            || credential
                .status
                .get("revoked_at")
                .is_some_and(|value| !value.is_null())
        {
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
        for binding in self.list_resources(Some(ROLE_BINDING_MANIFEST))? {
            let binding_spec: RoleBindingSpec = decode(&binding.spec, "RoleBinding spec")?;
            if binding_spec
                .subjects
                .iter()
                .any(|path| path == &spec.subject)
            {
                let role = self.get_resource(&binding_spec.role)?;
                let role_spec: RoleSpec = decode(&role.spec, "Role spec")?;
                rules.extend(role_spec.rules.into_iter().map(|rule| Rule {
                    manifests: rule.manifests,
                    verbs: rule.verbs,
                    paths: rule.paths,
                }));
            }
        }
        let driver_path = if subject_resource.manifest == SERVICE_ACCOUNT_MANIFEST {
            self.connection
                .query_row(
                    "SELECT driver_path FROM driver_service_accounts
                     WHERE service_account_path=?",
                    [&spec.subject],
                    |row| row.get(0),
                )
                .optional()?
        } else {
            None
        };
        Ok(AuthContext {
            subject: Subject {
                path: subject_resource.path,
                manifest: subject_resource.manifest,
            },
            rules,
            driver_path,
            driver_generation,
        })
    }

    pub fn revoke_credential(&mut self, path: &str) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut resource = resource_in(&tx, path)?;
        require_manifest(&resource, CREDENTIAL_MANIFEST)?;
        resource.status = json!({
            "state": STATE_AVAILABLE,
            "revoked_at": Utc::now()
        });
        let now = Utc::now();
        tx.execute(
            "UPDATE resources SET status_json=?,updated_at=? WHERE path=?",
            params![serde_json::to_string(&resource.status)?, stamp(now), path],
        )?;
        resource = resource_in(&tx, path)?;
        append_event(&tx, EventType::Updated, &resource, now)?;
        tx.commit()?;
        Ok(resource)
    }
}

const DELIVERY_SELECT: &str =
    "SELECT id,driver_path,generation,work_json,status,created_at,acked_at,completed_at
     FROM driver_deliveries";

struct BuiltinDocuments {
    manifest: &'static str,
    resources: &'static [&'static str],
}

fn builtin_documents() -> [BuiltinDocuments; 12] {
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
            resources: &[],
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
            manifest: include_str!("../../../builtins/role-binding/manifest.json"),
            resources: &[
                include_str!(
                    "../../../builtins/role-binding/resources/relations/role-binding-role.json"
                ),
                include_str!(
                    "../../../builtins/role-binding/resources/relations/role-binding-subject.json"
                ),
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
        let tx = connection.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
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
        manifest: row.get(1)?,
        name: row.get(2)?,
        spec: json_from_row(row, 3)?,
        status: json_from_row(row, 4)?,
        revision: row.get(5)?,
        created_at: time_from_row(row, 6)?,
        updated_at: time_from_row(row, 7)?,
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

fn delivery_from_row(row: &Row<'_>) -> rusqlite::Result<DriverDelivery> {
    let status: String = row.get(4)?;
    Ok(DriverDelivery {
        id: uuid_from_row(row, 0)?,
        driver_path: row.get(1)?,
        generation: row.get(2)?,
        work: json_from_row(row, 3)?,
        status: match status.as_str() {
            "pending" => DeliveryStatus::Pending,
            "acked" => DeliveryStatus::Acked,
            "completed" => DeliveryStatus::Completed,
            other => {
                return Err(from_sql(
                    4,
                    std::io::Error::other(format!("invalid delivery status {other}")),
                ))
            }
        },
        created_at: time_from_row(row, 5)?,
        acked_at: optional_time_from_row(row, 6)?,
        completed_at: optional_time_from_row(row, 7)?,
    })
}

fn delivery_in(tx: &Transaction<'_>, id: Uuid) -> Result<DriverDelivery, StoreError> {
    tx.query_row(
        &format!("{DELIVERY_SELECT} WHERE id=?"),
        [id.to_string()],
        delivery_from_row,
    )
    .optional()?
    .ok_or_else(|| StoreError::NotFound(format!("Driver delivery {id}")))
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
) -> Result<(PlannedResource, Value), StoreError> {
    let mut planned = planned.clone();
    if planned.path == MANIFEST_MANIFEST && planned.manifest == MANIFEST_MANIFEST {
        let manifest_spec: ManifestSpec = decode(&planned.spec, "Manifest spec")?;
        ensure_state(&mut planned.spec, &manifest_spec.default_state)?;
        let mut status = planned.status.clone();
        ensure_state(&mut status, &manifest_spec.initial_state)?;
        return Ok((planned, status));
    }
    let manifest = resource_in(tx, &planned.manifest)?;
    require_manifest(&manifest, MANIFEST_MANIFEST)?;
    let manifest_spec: ManifestSpec = decode(&manifest.spec, "Manifest spec")?;
    ensure_state(&mut planned.spec, &manifest_spec.default_state)?;
    let mut status = planned.status.clone();
    if status.is_null() {
        status = json!({});
    }
    ensure_state(&mut status, &manifest_spec.initial_state)?;
    Ok((planned, status))
}

fn planned_from_resource(resource: Resource) -> PlannedResource {
    PlannedResource {
        path: resource.path,
        manifest: resource.manifest,
        name: resource.name,
        spec: resource.spec,
        status: resource.status,
    }
}

fn validate_against_manifest(
    tx: &Transaction<'_>,
    manifest_path: &str,
    spec: &Value,
    status: &Value,
) -> Result<(), StoreError> {
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
    validate_resource_state("Resource spec", spec, &definition)?;
    validate_resource_state("Resource status", status, &definition)?;
    let mut business_spec = spec.clone();
    business_spec
        .as_object_mut()
        .ok_or_else(|| StoreError::Invalid("Resource spec must be an object".into()))?
        .remove("state");
    validate_json_schema("Resource spec", &definition.resource_schema, &business_spec)
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
    status: &Value,
    protected: bool,
    managed_by: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    validate_against_manifest(tx, &planned.manifest, &planned.spec, status)?;
    tx.execute(
        "INSERT INTO resources(
            path,manifest_path,name,spec_json,status_json,revision,protected,managed_by,
            created_at,updated_at
         ) VALUES (?,?,?,?,?,0,?,?,?,?)",
        params![
            planned.path,
            planned.manifest,
            planned.name,
            serde_json::to_string(&planned.spec)?,
            serde_json::to_string(status)?,
            protected,
            managed_by,
            stamp(now),
            stamp(now)
        ],
    )
    .map_err(|error| constraint(error, "Resource already exists"))?;
    Ok(())
}

fn project_resource(
    tx: &Transaction<'_>,
    planned: &PlannedResource,
    status: &Value,
    owner_manifest: &str,
    _now: DateTime<Utc>,
) -> Result<(), StoreError> {
    match planned.manifest.as_str() {
        MANIFEST_MANIFEST => {
            let spec: ManifestSpec = decode(&planned.spec, "Manifest spec")?;
            validate_manifest_states(&spec)?;
            tx.execute(
                "INSERT INTO manifest_index(manifest_path,version) VALUES (?,?)",
                params![planned.path, spec.version],
            )?;
        }
        RELATION_MANIFEST => {
            let spec: RelationSpec = decode(&planned.spec, "Relation spec")?;
            tx.execute(
                "INSERT INTO relation_index(
                    relation_path,owner_manifest_path,role,relation_type,ensure,on_source_delete
                 ) VALUES (?,?,?,?,?,?)",
                params![
                    planned.path,
                    owner_manifest,
                    spec.role.map(relation_role),
                    relation_type(spec.relation_type),
                    spec.ensure,
                    on_source_delete(spec.on_source_delete)
                ],
            )?;
        }
        ACTION_MANIFEST => {
            let _: ActionSpec = decode(&planned.spec, "Action spec")?;
            tx.execute(
                "INSERT INTO action_index(action_path,owner_manifest_path) VALUES (?,?)",
                params![planned.path, owner_manifest],
            )?;
        }
        DRIVER_MANIFEST => {
            let _: DriverSpec = decode(&planned.spec, "Driver spec")?;
            let driver_status: DriverStatus = decode(status, "Driver status")?;
            tx.execute(
                "INSERT INTO driver_runtime(
                    driver_path,owner_manifest_path,generation
                 ) VALUES (?,?,?)",
                params![planned.path, owner_manifest, driver_status.generation],
            )?;
        }
        LINK_MANIFEST => {
            project_link(tx, planned)?;
        }
        RUN_MANIFEST => {
            project_run(tx, planned, status)?;
        }
        ROLE_MANIFEST => {
            let _: RoleSpec = decode(&planned.spec, "Role spec")?;
        }
        ROLE_BINDING_MANIFEST => {
            let _: RoleBindingSpec = decode(&planned.spec, "RoleBinding spec")?;
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

fn validate_resource_state(
    kind: &str,
    document: &Value,
    manifest: &ManifestSpec,
) -> Result<(), StoreError> {
    let state = document_state(document)
        .ok_or_else(|| StoreError::Invalid(format!("{kind} must contain string state")))?;
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

fn project_link(tx: &Transaction<'_>, planned: &PlannedResource) -> Result<(), StoreError> {
    let spec: LinkSpec = decode(&planned.spec, "Link spec")?;
    let relation = resource_in(tx, &spec.relation)?;
    require_manifest(&relation, RELATION_MANIFEST)?;
    if spec.source.is_none() && spec.target.is_none() {
        return Err(StoreError::Invalid(
            "Link source and target cannot both be null".into(),
        ));
    }
    if let Some(source) = &spec.source {
        let source = resource_in(tx, source)?;
        let relation_spec: RelationSpec = decode(&relation.spec, "Relation spec")?;
        if !relation_spec
            .sources
            .iter()
            .any(|selector| selector.matches(&source))
        {
            return Err(StoreError::Invalid(format!(
                "Link source {} is not accepted by Relation {}",
                source.path, relation.path
            )));
        }
    }
    if let Some(target) = &spec.target {
        let target = resource_in(tx, target)?;
        let relation_spec: RelationSpec = decode(&relation.spec, "Relation spec")?;
        if !relation_spec
            .targets
            .iter()
            .any(|selector| selector.matches(&target))
        {
            return Err(StoreError::Invalid(format!(
                "Link target {} is not accepted by Relation {}",
                target.path, relation.path
            )));
        }
    }
    let relation_spec: RelationSpec = decode(&relation.spec, "Relation spec")?;
    enforce_cardinality(tx, &spec, relation_spec.relation_type, None)?;
    validate_json_schema(
        "Link metadata",
        &relation_spec.metadata_schema,
        &spec.metadata,
    )?;
    tx.execute(
        "INSERT INTO link_index(link_path,relation_path,source_path,target_path)
         VALUES (?,?,?,?)",
        params![planned.path, spec.relation, spec.source, spec.target],
    )?;
    Ok(())
}

fn project_run(
    tx: &Transaction<'_>,
    planned: &PlannedResource,
    status: &Value,
) -> Result<(), StoreError> {
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
    let run_status: RunStatus = decode(status, "Run status")?;
    let mut stored_spec = planned.spec.clone();
    stored_spec
        .as_object_mut()
        .ok_or_else(|| StoreError::Invalid("Run spec must be an object".into()))?
        .insert("driver".into(), Value::String(driver_path.clone()));
    tx.execute(
        "UPDATE resources SET spec_json=? WHERE path=?",
        params![serde_json::to_string(&stored_spec)?, planned.path],
    )?;
    tx.execute(
        "INSERT INTO run_runtime(
            run_path,request_id,resource_path,action_path,driver_path,driver_generation
         ) VALUES (?,?,?,?,?,?)",
        params![
            planned.path,
            spec.request_id.to_string(),
            spec.resource,
            spec.action,
            driver_path,
            run_status.driver_generation
        ],
    )?;
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
                    Some(&resource.path),
                    Some(&spec.service_account),
                    now,
                )?;
                tx.execute(
                    "INSERT INTO driver_service_accounts(
                        driver_path,service_account_path,link_path
                     ) VALUES (?,?,?)",
                    params![resource.path, spec.service_account, link_path],
                )?;
            }
        }
        ROLE_BINDING_MANIFEST => {
            let spec: RoleBindingSpec = decode(&resource.spec, "RoleBinding spec")?;
            if relation_path_for_role(tx, RelationRole::RoleBindingRole)?.is_some() {
                let link_path = format!("{}/links/role", resource.path);
                create_system_link(
                    tx,
                    &link_path,
                    RelationRole::RoleBindingRole,
                    Some(&resource.path),
                    Some(&spec.role),
                    now,
                )?;
                tx.execute(
                    "INSERT INTO role_binding_roles(role_binding_path,role_path,link_path)
                     VALUES (?,?,?)",
                    params![resource.path, spec.role, link_path],
                )?;
            }
            if relation_path_for_role(tx, RelationRole::RoleBindingSubject)?.is_some() {
                for (index, subject) in spec.subjects.iter().enumerate() {
                    let link_path = format!("{}/links/subjects/{index}", resource.path);
                    create_system_link(
                        tx,
                        &link_path,
                        RelationRole::RoleBindingSubject,
                        Some(&resource.path),
                        Some(subject),
                        now,
                    )?;
                    tx.execute(
                        "INSERT INTO role_binding_subjects(
                            role_binding_path,subject_path,link_path
                         ) VALUES (?,?,?)",
                        params![resource.path, subject, link_path],
                    )?;
                }
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
                        Some(&resource.path),
                        Some(target),
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
    source: Option<&str>,
    target: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<Resource>, StoreError> {
    let Some(relation) = relation_path_for_role(tx, role)? else {
        return Ok(None);
    };
    let spec = LinkSpec {
        relation,
        source: source.map(str::to_owned),
        target: target.map(str::to_owned),
        metadata: json!({}),
    };
    let planned = PlannedResource {
        path: path.into(),
        manifest: LINK_MANIFEST.into(),
        name: path.rsplit('/').next().unwrap_or("link").into(),
        spec: serde_json::to_value(&spec)?,
        status: serde_json::to_value(&spec)?,
    };
    if tx
        .query_row("SELECT 1 FROM resources WHERE path=?", [path], |_| Ok(()))
        .optional()?
        .is_some()
    {
        return Ok(Some(resource_in(tx, path)?));
    }
    // A Manifest-resource relationship only applies when the declared selectors
    // accept both endpoints. System Manifest Resources themselves are not
    // packaged Resources of another business Manifest.
    if let (Some(source), Some(target)) = (source, target) {
        let relation_resource = resource_in(tx, &spec.relation)?;
        let relation_spec: RelationSpec = decode(&relation_resource.spec, "Relation spec")?;
        let source_resource = resource_in(tx, source)?;
        let target_resource = resource_in(tx, target)?;
        if !relation_spec
            .sources
            .iter()
            .any(|selector| selector.matches(&source_resource))
            || !relation_spec
                .targets
                .iter()
                .any(|selector| selector.matches(&target_resource))
        {
            return Ok(None);
        }
    }
    let (planned, status) = normalized_initial_documents(tx, &planned)?;
    insert_resource_row(tx, &planned, &status, true, "system", now)?;
    project_link(tx, &planned)?;
    let resource = resource_in(tx, path)?;
    append_event(tx, EventType::Created, &resource, now)?;
    Ok(Some(resource))
}

fn relation_path_for_role(
    tx: &Transaction<'_>,
    role: RelationRole,
) -> Result<Option<String>, StoreError> {
    tx.query_row(
        "SELECT relation_path FROM relation_index WHERE role=?",
        [relation_role(role)],
        |row| row.get(0),
    )
    .optional()
    .map_err(StoreError::from)
}

fn relation_role(role: RelationRole) -> &'static str {
    match role {
        RelationRole::ManifestResource => "manifest_resource",
        RelationRole::PackageManifest => "package_manifest",
        RelationRole::ResourceManifest => "resource_manifest",
        RelationRole::RunResource => "run_resource",
        RelationRole::RunAction => "run_action",
        RelationRole::RunDriver => "run_driver",
        RelationRole::DriverServiceAccount => "driver_service_account",
        RelationRole::RoleBindingRole => "role_binding_role",
        RelationRole::RoleBindingSubject => "role_binding_subject",
    }
}

fn relation_type(value: kas_core::RelationType) -> &'static str {
    match value {
        kas_core::RelationType::OneToOne => "one_to_one",
        kas_core::RelationType::OneToMany => "one_to_many",
        kas_core::RelationType::ManyToOne => "many_to_one",
        kas_core::RelationType::ManyToMany => "many_to_many",
    }
}

fn on_source_delete(value: kas_core::OnSourceDelete) -> &'static str {
    match value {
        kas_core::OnSourceDelete::Unlink => "unlink",
        kas_core::OnSourceDelete::Cascade => "cascade",
    }
}

fn enforce_cardinality(
    tx: &Transaction<'_>,
    link: &LinkSpec,
    relation_type: kas_core::RelationType,
    excluding: Option<&str>,
) -> Result<(), StoreError> {
    let excluded = excluding.unwrap_or("");
    let source_taken: bool = if let Some(source) = &link.source {
        tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM link_index
                WHERE relation_path=? AND source_path=? AND link_path<>?
             )",
            params![link.relation, source, excluded],
            |row| row.get(0),
        )?
    } else {
        false
    };
    let target_taken: bool = if let Some(target) = &link.target {
        tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM link_index
                WHERE relation_path=? AND target_path=? AND link_path<>?
             )",
            params![link.relation, target, excluded],
            |row| row.get(0),
        )?
    } else {
        false
    };
    let invalid = match relation_type {
        kas_core::RelationType::OneToOne => source_taken || target_taken,
        kas_core::RelationType::OneToMany => target_taken,
        kas_core::RelationType::ManyToOne => source_taken,
        kas_core::RelationType::ManyToMany => false,
    };
    if invalid {
        return Err(StoreError::Conflict(
            "Relation cardinality would be violated".into(),
        ));
    }
    Ok(())
}

fn refresh_projection(
    tx: &Transaction<'_>,
    resource: &Resource,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    match resource.manifest.as_str() {
        LINK_MANIFEST => {
            tx.execute("DELETE FROM link_index WHERE link_path=?", [&resource.path])?;
            project_link(
                tx,
                &PlannedResource {
                    path: resource.path.clone(),
                    manifest: resource.manifest.clone(),
                    name: resource.name.clone(),
                    spec: resource.spec.clone(),
                    status: resource.status.clone(),
                },
            )?;
        }
        ROLE_BINDING_MANIFEST => {
            let link_paths = {
                let mut statement = tx.prepare(
                    "SELECT link_path FROM role_binding_roles WHERE role_binding_path=?
                     UNION ALL
                     SELECT link_path FROM role_binding_subjects WHERE role_binding_path=?",
                )?;
                let rows = statement
                    .query_map(params![resource.path, resource.path], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            for link_path in link_paths {
                hard_delete_resource(tx, &link_path, now)?;
            }
            project_declared_relationships(tx, &planned_from_resource(resource.clone()), now)?;
        }
        _ => {}
    }
    Ok(())
}

fn driver_path_for_manifest(
    tx: &Transaction<'_>,
    manifest_path: &str,
) -> Result<Option<String>, StoreError> {
    tx.query_row(
        "SELECT driver_path FROM driver_runtime WHERE owner_manifest_path=?",
        [manifest_path],
        |row| row.get(0),
    )
    .optional()
    .map_err(StoreError::from)
}

fn driver_for_resource(
    tx: &Transaction<'_>,
    resource: &Resource,
) -> Result<Option<String>, StoreError> {
    if resource.manifest == LINK_MANIFEST {
        let link: LinkSpec = decode(&resource.spec, "Link spec")?;
        return tx
            .query_row(
                "SELECT d.driver_path FROM relation_index r
                 JOIN driver_runtime d ON d.owner_manifest_path=r.owner_manifest_path
                 WHERE r.relation_path=?",
                [link.relation],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from);
    }
    driver_path_for_manifest(tx, &resource.manifest)
}

fn enqueue_if_drifted(
    tx: &Transaction<'_>,
    resource: &Resource,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    if resource.spec == resource.status {
        tx.execute(
            "DELETE FROM reconcile_queue WHERE resource_path=?",
            [&resource.path],
        )?;
        return Ok(());
    }
    let Some(driver) = driver_for_resource(tx, resource)? else {
        return Ok(());
    };
    tx.execute(
        "INSERT INTO reconcile_queue(
            resource_path,driver_path,reason,available_at,updated_at
         ) VALUES (?,?,?,?,?)
         ON CONFLICT(resource_path) DO UPDATE SET
            driver_path=excluded.driver_path,
            reason=excluded.reason,
            available_at=excluded.available_at,
            updated_at=excluded.updated_at",
        params![resource.path, driver, reason, stamp(now), stamp(now)],
    )?;
    Ok(())
}

fn reconcile_ensures(tx: &Transaction<'_>, now: DateTime<Utc>) -> Result<(), StoreError> {
    let relation_paths = {
        let mut statement = tx.prepare(
            "SELECT relation_path FROM relation_index
             WHERE ensure=1 AND relation_type='one_to_one' ORDER BY relation_path",
        )?;
        let values = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    for relation_path in relation_paths {
        let relation = resource_in(tx, &relation_path)?;
        let spec: RelationSpec = decode(&relation.spec, "Relation spec")?;
        let resources = all_resources_in(tx)?;
        for source in resources.iter().filter(|resource| {
            spec.sources
                .iter()
                .any(|selector| selector.matches(resource))
        }) {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM link_index WHERE relation_path=? AND source_path=?
                 )",
                params![relation_path, source.path],
                |row| row.get(0),
            )?;
            if exists {
                continue;
            }
            let link_path = format!("{}/links/ensure-{}", relation_path, Uuid::new_v4());
            let link = LinkSpec {
                relation: relation_path.clone(),
                source: Some(source.path.clone()),
                target: None,
                metadata: json!({}),
            };
            let planned = PlannedResource {
                path: link_path,
                manifest: LINK_MANIFEST.into(),
                name: "ensure".into(),
                spec: serde_json::to_value(&link)?,
                status: json!({"state": kas_core::STATE_PENDING}),
            };
            let (planned, status) = normalized_initial_documents(tx, &planned)?;
            insert_resource_row(tx, &planned, &status, true, "system", now)?;
            project_link(tx, &planned)?;
            let resource = resource_in(tx, &planned.path)?;
            append_event(tx, EventType::Created, &resource, now)?;
            enqueue_if_drifted(tx, &resource, "relation_ensure", now)?;
        }
    }
    Ok(())
}

fn reconcile_platform_state(store: &mut Store) -> Result<(), StoreError> {
    let tx = store.connection.transaction()?;
    let now = Utc::now();
    for resource in all_resources_in(&tx)? {
        enqueue_if_drifted(&tx, &resource, "startup_resync", now)?;
    }
    reconcile_ensures(&tx, now)?;
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
            let (resource, status) = normalized_initial_documents(tx, &resource)?;
            validate_against_manifest(tx, &resource.manifest, &resource.spec, &status)?;
            insert_resource_row(tx, &resource, &status, false, "driver", now)?;
            let owner_manifest = tx
                .query_row(
                    "SELECT owner_manifest_path FROM driver_runtime WHERE driver_path=?",
                    [driver_path],
                    |row| row.get::<_, String>(0),
                )
                .unwrap_or_default();
            project_resource(tx, &resource, &status, &owner_manifest, now)?;
            let created = resource_in(tx, &resource.path)?;
            project_declared_relationships(tx, &planned_from_resource(created.clone()), now)?;
            append_event(tx, EventType::Created, &created, now)?;
            enqueue_if_drifted(tx, &created, "driver_created", now)?;
            Ok(serde_json::to_value(created)?)
        }
        Mutation::UpdateResource {
            resource_path,
            expected_revision,
            spec,
        } => {
            let current = resource_in(tx, &resource_path)?;
            validate_against_manifest(tx, &current.manifest, &spec, &current.status)?;
            let changed = tx.execute(
                "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=?
                 WHERE path=? AND revision=? AND protected=0",
                params![
                    serde_json::to_string(&spec)?,
                    stamp(now),
                    resource_path,
                    expected_revision
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::Conflict("Resource revision is stale".into()));
            }
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
            if resource.revision != expected_revision || is_protected(tx, &resource_path)? {
                return Err(StoreError::Conflict(
                    "Resource revision is stale or protected".into(),
                ));
            }
            set_document_state(&mut resource.spec, STATE_DELETED)?;
            tx.execute(
                "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=? WHERE path=?",
                params![
                    serde_json::to_string(&resource.spec)?,
                    stamp(now),
                    resource_path
                ],
            )?;
            resource = resource_in(tx, &resource_path)?;
            append_event(tx, EventType::Updated, &resource, now)?;
            enqueue_if_drifted(tx, &resource, "driver_delete_requested", now)?;
            Ok(serde_json::to_value(resource)?)
        }
        Mutation::UpdateResourceStatus {
            resource_path,
            expected_revision,
            status,
        } => {
            assert_driver_owns(tx, &resource_path, driver_path, generation)?;
            let current = resource_in(tx, &resource_path)?;
            validate_against_manifest(tx, &current.manifest, &current.spec, &status)?;
            let changed = tx.execute(
                "UPDATE resources SET status_json=?,updated_at=?
                 WHERE path=? AND revision=?",
                params![
                    serde_json::to_string(&status)?,
                    stamp(now),
                    resource_path,
                    expected_revision
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::Conflict("Resource revision is stale".into()));
            }
            let updated = resource_in(tx, &resource_path)?;
            refresh_projection(tx, &updated, now)?;
            append_event(tx, EventType::Updated, &updated, now)?;
            if updated.spec == updated.status
                && document_state(&updated.spec) == Some(STATE_DELETED)
            {
                hard_delete_resource(tx, &resource_path, now)?;
            } else {
                enqueue_if_drifted(tx, &updated, "driver_status_updated", now)?;
            }
            Ok(serde_json::to_value(updated)?)
        }
        Mutation::CompleteRun { run_path, result } => {
            let mut run = resource_in(tx, &run_path)?;
            require_manifest(&run, RUN_MANIFEST)?;
            let mut status: RunStatus = decode(&run.status, "Run status")?;
            if status.driver_generation != Some(generation) {
                return Err(StoreError::Conflict("Run generation is stale".into()));
            }
            apply_run_result(&mut status, result);
            update_status_document(tx, &run_path, &status, now)?;
            tx.execute(
                "UPDATE run_runtime SET finished_at=? WHERE run_path=? AND driver_path=?",
                params![stamp(now), run_path, driver_path],
            )?;
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

    let linked = {
        let mut statement = tx.prepare(
            "SELECT l.link_path,l.source_path,l.target_path,r.on_source_delete
             FROM link_index l
             JOIN relation_index r ON r.relation_path=l.relation_path
             WHERE (l.source_path=? OR l.target_path=?) AND l.link_path<>?",
        )?;
        let rows = statement
            .query_map(params![path, path, path], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let cascade_targets = linked
        .iter()
        .filter_map(|(_, source, target, on_delete)| {
            (source.as_deref() == Some(path) && on_delete == "cascade")
                .then(|| target.clone())
                .flatten()
        })
        .filter(|target| target != path)
        .collect::<Vec<_>>();
    for (link, _, _, _) in linked {
        if tx
            .query_row("SELECT 1 FROM resources WHERE path=?", [&link], |_| Ok(()))
            .optional()?
            .is_some()
        {
            hard_delete_resource(tx, &link, now)?;
        }
    }
    for target in cascade_targets {
        if tx
            .query_row(
                "SELECT 1 FROM resources WHERE path=?",
                [&target],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            hard_delete_resource(tx, &target, now)?;
        }
    }
    let run_paths = {
        let mut statement = tx.prepare("SELECT run_path FROM run_runtime WHERE resource_path=?")?;
        let rows = statement
            .query_map([path], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for run in run_paths {
        hard_delete_resource(tx, &run, now)?;
    }
    if resource.manifest == LINK_MANIFEST {
        tx.execute(
            "DELETE FROM driver_service_accounts WHERE link_path=?",
            [path],
        )?;
        tx.execute("DELETE FROM role_binding_roles WHERE link_path=?", [path])?;
        tx.execute(
            "DELETE FROM role_binding_subjects WHERE link_path=?",
            [path],
        )?;
    }
    append_deleted_event(tx, &resource, now)?;
    tx.execute("DELETE FROM resources WHERE path=?", [path])?;
    Ok(())
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
    let status: DriverStatus = decode(&driver.status, "Driver status")?;
    if status.generation != generation || status.state != DriverState::Ready {
        return Err(StoreError::Conflict("Driver generation is stale".into()));
    }
    Ok(())
}

fn update_status_document<T: Serialize>(
    tx: &Transaction<'_>,
    path: &str,
    status: &T,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE resources SET status_json=?,updated_at=? WHERE path=?",
        params![serde_json::to_string(status)?, stamp(now), path],
    )?;
    Ok(())
}

fn apply_run_result(status: &mut RunStatus, result: RunResult) {
    match result {
        RunResult::Succeeded { output } => {
            status.state = RunState::Succeeded;
            status.output = Some(output);
            status.error = None;
        }
        RunResult::Failed { error } => {
            status.state = RunState::Failed;
            status.output = None;
            status.error = Some(error);
        }
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
    let mut statement = tx.prepare(&format!("{RESOURCE_SELECT} ORDER BY created_at,path"))?;
    let rows = statement.query_map([], resource_from_row)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn is_protected(connection: &Connection, path: &str) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT protected FROM resources WHERE path=?",
            [path],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("Resource {path}")))
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

fn ensure_state(document: &mut Value, state: &str) -> Result<(), StoreError> {
    if document.is_null() {
        *document = json!({});
    }
    let object = document
        .as_object_mut()
        .ok_or_else(|| StoreError::Invalid("Resource document must be an object".into()))?;
    object
        .entry("state")
        .or_insert_with(|| Value::String(state.into()));
    Ok(())
}

fn set_document_state(document: &mut Value, state: &str) -> Result<(), StoreError> {
    if document.is_null() {
        *document = json!({});
    }
    document
        .as_object_mut()
        .ok_or_else(|| StoreError::Invalid("Resource document must be an object".into()))?
        .insert("state".into(), Value::String(state.into()));
    Ok(())
}

fn document_state(document: &Value) -> Option<&str> {
    document.get("state").and_then(Value::as_str)
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

fn optional_time_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let raw: Option<String> = row.get(index)?;
    raw.map(|value| {
        DateTime::parse_from_rfc3339(&value)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| from_sql(index, error))
    })
    .transpose()
}

fn uuid_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    let raw: String = row.get(index)?;
    Uuid::parse_str(&raw).map_err(|error| from_sql(index, error))
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
    use kas_core::{ManifestDefinition, PackageDefinition, ResourceDefinition};

    fn echo_manifest(ensure_peer: bool) -> PackageExpansion {
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
                manifest: RELATION_MANIFEST.into(),
                name: "peer".into(),
                spec: json!({
                    "sources": [{"manifest": "/manifests/test/echo"}],
                    "targets": [{"manifest": "/manifests/test/echo"}],
                    "type": if ensure_peer { "one_to_one" } else { "many_to_many" },
                    "ensure": ensure_peer,
                    "on_source_delete": "unlink",
                    "metadata_schema": {"type": "object"}
                }),
                status: json!({}),
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
        assert_eq!(root.spec["state"], STATE_AVAILABLE);
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
        let mut builtin_packages = std::collections::BTreeSet::from([package.path]);
        for manifest_path in [
            ACTION_MANIFEST,
            RELATION_MANIFEST,
            LINK_MANIFEST,
            DRIVER_MANIFEST,
            RUN_MANIFEST,
            USER_MANIFEST,
            SERVICE_ACCOUNT_MANIFEST,
            ROLE_MANIFEST,
            ROLE_BINDING_MANIFEST,
            CREDENTIAL_MANIFEST,
            PACKAGE_MANIFEST,
        ] {
            builtin_packages.insert(store.package_for_manifest(manifest_path).unwrap().path);
        }
        assert_eq!(builtin_packages.len(), 12);
        assert!(store
            .list_resources(Some(ROLE_MANIFEST))
            .unwrap()
            .iter()
            .any(|resource| {
                decode::<RoleSpec>(&resource.spec, "Role spec")
                    .is_ok_and(|role| role.system_role == Some(SystemRole::Admin))
            }));
    }

    #[test]
    fn generic_resource_creation_applies_manifest_states_and_schema() {
        let mut store = Store::memory().unwrap();
        store
            .install_package(
                echo_manifest(false),
                123,
                kas_core::MANIFEST_PACKAGE_MEDIA_TYPE,
            )
            .unwrap();

        let created = store
            .create_resource(PlannedResource {
                path: "/resources/test/echo-1".into(),
                manifest: "/manifests/test/echo".into(),
                name: "echo-1".into(),
                spec: json!({"label": "one"}),
                status: Value::Null,
            })
            .unwrap();
        assert_eq!(created.spec, json!({"label": "one", "state": "available"}));
        assert_eq!(created.status, json!({"state": "pending"}));
        assert_eq!(
            store
                .list_events(None, 100)
                .unwrap()
                .last()
                .unwrap()
                .resource_path,
            created.path
        );

        let invalid = store.create_resource(PlannedResource {
            path: "/resources/test/invalid".into(),
            manifest: "/manifests/test/echo".into(),
            name: "invalid".into(),
            spec: json!({"label": 7}),
            status: Value::Null,
        });
        assert!(matches!(invalid, Err(StoreError::Invalid(_))));

        let forged_package = store.create_resource(PlannedResource {
            path: "/packages/sha256/cafe".into(),
            manifest: PACKAGE_MANIFEST.into(),
            name: "sha256:cafe".into(),
            spec: json!({
                "digest": "sha256:cafe",
                "size_bytes": 1,
                "media_type": kas_core::MANIFEST_PACKAGE_MEDIA_TYPE
            }),
            status: Value::Null,
        });
        assert!(matches!(forged_package, Err(StoreError::Invalid(_))));

        let unknown_state = store.create_resource(PlannedResource {
            path: "/resources/test/unknown-state".into(),
            manifest: "/manifests/test/echo".into(),
            name: "unknown-state".into(),
            spec: json!({"label": "invalid", "state": "mystery"}),
            status: Value::Null,
        });
        assert!(matches!(unknown_state, Err(StoreError::Invalid(_))));
    }

    #[test]
    fn manifest_cannot_redeclare_platform_states() {
        let mut store = Store::memory().unwrap();
        let mut package = echo_manifest(false);
        package.resources[0].spec["states"] = json!([STATE_AVAILABLE]);

        let result = store.install_package(package, 123, kas_core::MANIFEST_PACKAGE_MEDIA_TYPE);
        assert!(matches!(result, Err(StoreError::Invalid(_))));
    }

    #[test]
    fn links_are_resources_and_ensure_creates_a_pending_partial_link() {
        let mut store = Store::memory().unwrap();
        store
            .install_package(
                echo_manifest(true),
                123,
                kas_core::MANIFEST_PACKAGE_MEDIA_TYPE,
            )
            .unwrap();
        let resource = store
            .create_resource(PlannedResource {
                path: "/resources/test/echo-1".into(),
                manifest: "/manifests/test/echo".into(),
                name: "echo-1".into(),
                spec: json!({"label": "one"}),
                status: Value::Null,
            })
            .unwrap();

        let links = store.links_for_resource(&resource.path).unwrap();
        let ensured = links
            .iter()
            .find(|link| {
                link.spec["relation"] == "/manifests/test/echo/relations/peer"
                    && link.spec["source"] == resource.path
            })
            .unwrap();
        assert_eq!(ensured.manifest, LINK_MANIFEST);
        assert!(ensured.spec["target"].is_null());
        assert_eq!(ensured.spec["state"], STATE_AVAILABLE);
        assert_eq!(ensured.status["state"], kas_core::STATE_PENDING);
    }
}
