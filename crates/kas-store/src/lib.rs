use std::path::Path;

use chrono::{DateTime, Utc};
use kas_auth::{
    AuthContext, CreateRole, CreateRoleBinding, CreateServiceAccount, CreateUser, IssuedCredential,
    Role, RoleBinding, Rule, ServiceAccount, Subject, SubjectKind, User, SYSTEM_ADMIN_ROLE,
};
use kas_core::{
    Action, CreateLink, CreateManifest, CreateResource, CreateRun, DeliveryStatus, Driver,
    DriverDelivery, DriverReady, DriverState, DriverWork, Event, EventFilter, EventType, FinishRun,
    Link, LinkFilter, Manifest, Mutation, ObjectKind, ObjectRef, Resource, Run, RunResult,
    RunStatus, UpdateResource, UpdateResourceStatus,
};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

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

pub const LATEST_SCHEMA_VERSION: u32 = 6;

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
];

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
        Ok(Self { connection })
    }

    pub fn memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        configure(&connection)?;
        let mut store = Self { connection };
        migrate_connection(&mut store.connection)?;
        Ok(store)
    }

    pub fn create_manifest(&mut self, input: CreateManifest) -> Result<Manifest, StoreError> {
        validate_name("Manifest name", &input.name)?;
        validate_permission_segment("Manifest name", &input.name)?;
        if let Some(driver) = &input.driver {
            validate_name("Driver name", driver)?;
        } else if !input.actions.is_empty() {
            return Err(StoreError::Invalid(
                "A Manifest that declares Actions must also declare a Driver".into(),
            ));
        }
        validate_manifest_contract(&input.resource_schema)?;
        if input.version == 0 {
            return Err(StoreError::Invalid(
                "Manifest version must start at 1".into(),
            ));
        }
        let now = Utc::now();
        validate_object_path("Manifest path", &input.path)?;
        let manifest_path = input.path.clone();
        let actions_json = serde_json::to_string(&input.actions)?;
        let schema_json = serde_json::to_string(&input.resource_schema)?;
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO manifests(id,name,version,description,resource_schema_json,actions_json,driver_name,created_at)
             VALUES (?,?,?,?,?,?,?,?)",
            params![
                manifest_path.to_string(), input.name, input.version, input.description,
                schema_json, actions_json, input.driver.as_deref(), stamp(now)
            ],
        )
        .map_err(|error| constraint(error, "Manifest name and version already exist"))?;
        if let Some(driver_name) = &input.driver {
            let driver_path = format!("/drivers/{}", input.name);
            tx.execute(
            "INSERT INTO drivers(id,manifest_path,name,state,generation,process_id,metadata_json,started_at,heartbeat_at,stopped_at,error,created_at,updated_at)
             VALUES (?,?,?,'stopped',0,NULL,'{}',NULL,NULL,?,NULL,?,?)",
            params![driver_path.to_string(), manifest_path.to_string(), driver_name, stamp(now), stamp(now), stamp(now)],
            )?;
            let service_account_path = format!("{driver_path}/service-account");
            let role_path = format!("/roles/system/drivers/{}", input.name);
            let role_binding_path = format!("/role-bindings/system/drivers/{}", input.name);
            let identity_name = format!("system:driver:{driver_path}");
            tx.execute(
            "INSERT INTO service_accounts(id,name,driver_path,managed_by,created_at) VALUES (?,?,?,'system',?)",
            params![service_account_path.to_string(), identity_name, driver_path.to_string(), stamp(now)],
            )?;
            tx.execute(
                "INSERT INTO roles(id,name,description,rules_json,managed_by,created_at,updated_at)
             VALUES (?,?,?,?,'system',?,?)",
                params![
                    role_path.to_string(),
                    format!("system:driver-role:{driver_path}"),
                    "Driver runtime access",
                    serde_json::to_string(&driver_rules())?,
                    stamp(now),
                    stamp(now)
                ],
            )?;
            tx.execute(
            "INSERT INTO role_bindings(id,name,role_path,managed_by,created_at) VALUES (?,?,?,'system',?)",
            params![role_binding_path.to_string(), format!("system:driver:{driver_path}"), role_path.to_string(), stamp(now)],
            )?;
            tx.execute(
            "INSERT INTO role_binding_subjects(role_binding_path,subject_kind,subject_path) VALUES (?,'service_account',?)",
            params![role_binding_path.to_string(), service_account_path.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(Manifest {
            path: manifest_path,
            name: input.name,
            version: input.version,
            description: input.description,
            resource_schema: input.resource_schema,
            actions: input.actions,
            driver: input.driver,
            created_at: now,
        })
    }

    pub fn list_manifests(&self) -> Result<Vec<Manifest>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,name,version,description,resource_schema_json,actions_json,driver_name,created_at
             FROM manifests ORDER BY name,version",
        )?;
        let rows = statement.query_map([], manifest_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_manifest(&self, path: &str) -> Result<Manifest, StoreError> {
        self.connection
            .query_row(
                "SELECT id,name,version,description,resource_schema_json,actions_json,driver_name,created_at FROM manifests WHERE id=?",
                [path],
                manifest_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Manifest {path}")))
    }

    pub fn create_resource(&mut self, input: CreateResource) -> Result<Resource, StoreError> {
        validate_object_path("Resource path", &input.path)?;
        validate_name("Resource name", &input.name)?;
        let schema: String = self
            .connection
            .query_row(
                "SELECT resource_schema_json FROM manifests WHERE id=?",
                [input.manifest_path.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Manifest {}", input.manifest_path)))?;
        validate_json_schema(
            "Resource spec",
            &serde_json::from_str(&schema)?,
            &input.spec,
        )?;
        let now = Utc::now();
        let path = input.path.clone();
        let spec = serde_json::to_string(&input.spec)?;
        let tx = self.connection.transaction()?;
        tx.execute(
                "INSERT INTO resources(id,manifest_path,name,spec_json,status_json,revision,created_at,updated_at)
                 VALUES (?,?,?,?,'{}',0,?,?)",
                params![path, input.manifest_path, input.name, spec, stamp(now), stamp(now)],
            )
            .map_err(|error| constraint(error, "Manifest does not exist"))?;
        let resource = Resource {
            path: path.clone(),
            manifest_path: input.manifest_path,
            name: input.name,
            spec: input.spec,
            status: Value::Object(Default::default()),
            revision: 0,
            created_at: now,
            updated_at: now,
        };
        append_lifecycle_event(
            &tx,
            EventType::Created,
            ObjectKind::Resource,
            &path,
            Some(resource.manifest_path.clone()),
            Some(resource.revision),
            &resource,
            now,
        )?;
        tx.commit()?;
        Ok(resource)
    }

    pub fn update_resource(
        &mut self,
        resource_path: &str,
        input: UpdateResource,
    ) -> Result<Resource, StoreError> {
        let schema: String = self
            .connection
            .query_row(
                "SELECT m.resource_schema_json FROM resources r JOIN manifests m ON m.id=r.manifest_path WHERE r.id=?",
                [resource_path.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Resource {resource_path}")))?;
        validate_json_schema(
            "Resource spec",
            &serde_json::from_str(&schema)?,
            &input.spec,
        )?;
        let tx = self.connection.transaction()?;
        let now = Utc::now();
        let changed = tx.execute(
            "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=? WHERE id=? AND revision=?",
            params![
                serde_json::to_string(&input.spec)?,
                stamp(now),
                resource_path.to_string(),
                input.expected_revision
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(format!(
                "Resource {resource_path} revision is stale"
            )));
        }
        let resource = tx.query_row(
            RESOURCE_SELECT_BY_ID,
            [resource_path.to_string()],
            resource_from_row,
        )?;
        append_lifecycle_event(
            &tx,
            EventType::Updated,
            ObjectKind::Resource,
            resource_path,
            Some(resource.manifest_path.clone()),
            Some(resource.revision),
            &resource,
            now,
        )?;
        tx.commit()?;
        Ok(resource)
    }

    pub fn list_resources(&self) -> Result<Vec<Resource>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,manifest_path,name,spec_json,status_json,revision,created_at,updated_at
             FROM resources ORDER BY created_at,id",
        )?;
        let rows = statement.query_map([], resource_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_resource(&self, resource_path: &str) -> Result<Resource, StoreError> {
        self.connection
            .query_row(
                RESOURCE_SELECT_BY_ID,
                [resource_path.to_string()],
                resource_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Resource {resource_path}")))
    }

    pub fn update_resource_status(
        &mut self,
        resource_path: &str,
        input: UpdateResourceStatus,
    ) -> Result<Resource, StoreError> {
        let now = Utc::now();
        let status_json = serde_json::to_string(&input.status)?;
        let tx = self.connection.transaction()?;
        let changed = tx.execute(
            "UPDATE resources SET status_json=?,observed_revision=?,claimed_revision=NULL,
             claim_driver_generation=NULL,updated_at=?
             WHERE id=? AND revision=? AND claimed_revision=? AND claim_driver_generation=?
             AND EXISTS(
                SELECT 1 FROM drivers d
                WHERE d.id=? AND d.manifest_path=resources.manifest_path
                AND d.generation=? AND d.state='ready'
             )",
            params![
                status_json,
                input.observed_revision,
                stamp(now),
                resource_path.to_string(),
                input.observed_revision,
                input.observed_revision,
                input.driver_generation,
                input.driver_path.to_string(),
                input.driver_generation,
            ],
        )?;
        if changed != 1 {
            let resource = tx.query_row(
                RESOURCE_SELECT_BY_ID,
                [resource_path.to_string()],
                resource_from_row,
            )?;
            let observed_revision: i64 = tx.query_row(
                "SELECT observed_revision FROM resources WHERE id=?",
                [resource_path.to_string()],
                |row| row.get(0),
            )?;
            if observed_revision == input.observed_revision as i64
                && resource.status == input.status
            {
                return Ok(resource);
            }
            return Err(StoreError::Conflict(
                "Reconciliation claim, Resource revision, or Driver generation is stale".into(),
            ));
        }
        let resource = tx.query_row(
            RESOURCE_SELECT_BY_ID,
            [resource_path.to_string()],
            resource_from_row,
        )?;
        append_lifecycle_event(
            &tx,
            EventType::Updated,
            ObjectKind::Resource,
            resource_path,
            Some(resource.manifest_path.clone()),
            Some(resource.revision),
            &resource,
            now,
        )?;
        tx.commit()?;
        Ok(resource)
    }

    pub fn finish_reconciliation_delivery(
        &mut self,
        delivery_id: Uuid,
        driver_path: &str,
        generation: u64,
        resource_path: &str,
        observed_revision: u64,
        status: Value,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let now = Utc::now();
        let changed = tx.execute(
            "UPDATE resources SET status_json=?,observed_revision=?,claimed_revision=NULL,
             claim_driver_generation=NULL,updated_at=?
             WHERE id=? AND revision=? AND claimed_revision=? AND claim_driver_generation=?
             AND EXISTS(
                SELECT 1 FROM drivers d
                WHERE d.id=? AND d.manifest_path=resources.manifest_path
                AND d.generation=? AND d.state='ready'
             )",
            params![
                serde_json::to_string(&status)?,
                observed_revision,
                stamp(now),
                resource_path.to_string(),
                observed_revision,
                observed_revision,
                generation,
                driver_path.to_string(),
                generation,
            ],
        )?;
        if changed != 1 {
            let existing: Option<(i64, String)> = tx
                .query_row(
                    "SELECT observed_revision,status_json FROM resources WHERE id=?",
                    [resource_path.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((existing_revision, existing_status)) = existing else {
                return Err(StoreError::NotFound(format!("Resource {resource_path}")));
            };
            if existing_revision != observed_revision as i64
                || serde_json::from_str::<Value>(&existing_status)? != status
            {
                return Err(StoreError::Conflict(
                    "Reconciliation claim, Resource revision, or Driver generation is stale".into(),
                ));
            }
        } else {
            let resource = tx.query_row(
                RESOURCE_SELECT_BY_ID,
                [resource_path.to_string()],
                resource_from_row,
            )?;
            append_lifecycle_event(
                &tx,
                EventType::Updated,
                ObjectKind::Resource,
                resource_path,
                Some(resource.manifest_path.clone()),
                Some(resource.revision),
                &resource,
                now,
            )?;
        }
        complete_delivery_in_tx(&tx, delivery_id, driver_path, generation)?;
        tx.commit()?;
        self.get_resource(resource_path)
    }

    pub fn driver_for_manifest(&self, manifest_path: &str) -> Result<Option<Driver>, StoreError> {
        let manifest_exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM manifests WHERE id=?)",
            [manifest_path.to_string()],
            |row| row.get(0),
        )?;
        if !manifest_exists {
            return Err(StoreError::NotFound(format!("Manifest {manifest_path}")));
        }
        self.connection
            .query_row(
                DRIVER_SELECT_BY_MANIFEST,
                [manifest_path.to_string()],
                driver_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn get_driver(&self, driver_path: &str) -> Result<Driver, StoreError> {
        self.connection
            .query_row(
                DRIVER_SELECT_BY_ID,
                [driver_path.to_string()],
                driver_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Driver {driver_path}")))
    }

    pub fn start_driver(&mut self, driver_path: &str) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let tx = self.connection.transaction()?;
        let changed = tx.execute(
            "UPDATE drivers SET state='starting',generation=generation+1,process_id=NULL,
             metadata_json='{}',started_at=?,heartbeat_at=NULL,stopped_at=NULL,error=NULL,updated_at=?
             WHERE id=? AND state IN ('stopped','failed')",
            params![stamp(now), stamp(now), driver_path.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver can only start from stopped or failed".into(),
            ));
        }
        let running_ids = {
            let mut statement =
                tx.prepare("SELECT id FROM runs WHERE driver_path=? AND status='running'")?;
            let rows = statement
                .query_map([driver_path.to_string()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        tx.execute(
            "UPDATE runs SET status='queued',driver_generation=NULL,started_at=NULL
             WHERE driver_path=? AND status='running'",
            [driver_path.to_string()],
        )?;
        for id in running_ids {
            let run = tx.query_row(RUN_SELECT_BY_ID, [&id], run_from_row)?;
            let manifest_path = manifest_path_for_run(&tx, &id)?;
            append_lifecycle_event(
                &tx,
                EventType::Updated,
                ObjectKind::Run,
                &id,
                Some(manifest_path),
                None,
                &run,
                now,
            )?;
        }
        tx.execute(
            "UPDATE driver_deliveries SET status='completed',completed_at=?
             WHERE driver_path=? AND status!='completed'",
            params![stamp(now), driver_path.to_string()],
        )?;
        tx.commit()?;
        self.get_driver(driver_path)
    }

    pub fn mark_driver_ready(
        &mut self,
        driver_path: &str,
        input: DriverReady,
    ) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let metadata = serde_json::to_string(&input.metadata)?;
        let changed = self.connection.execute(
            "UPDATE drivers SET state='ready',process_id=?,metadata_json=?,heartbeat_at=?,updated_at=?
             WHERE id=? AND generation=? AND state='starting'",
            params![input.process_id, metadata, stamp(now), stamp(now), driver_path.to_string(), input.generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver generation is stale or not starting".into(),
            ));
        }
        self.get_driver(driver_path)
    }

    pub fn heartbeat_driver(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET heartbeat_at=?,updated_at=?
             WHERE id=? AND generation=? AND state='ready'",
            params![stamp(now), stamp(now), driver_path.to_string(), generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver generation is stale or not ready".into(),
            ));
        }
        self.get_driver(driver_path)
    }

    pub fn stop_driver(&mut self, driver_path: &str) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET state='stopping',updated_at=?
             WHERE id=? AND state IN ('starting','ready')",
            params![stamp(now), driver_path.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver can only stop from starting or ready".into(),
            ));
        }
        self.get_driver(driver_path)
    }

    pub fn mark_driver_stopped(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET state='stopped',process_id=NULL,heartbeat_at=NULL,stopped_at=?,updated_at=?
             WHERE id=? AND generation=? AND state='stopping'",
            params![stamp(now), stamp(now), driver_path.to_string(), generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver generation is stale or not stopping".into(),
            ));
        }
        self.get_driver(driver_path)
    }

    pub fn enqueue_run(&mut self, input: CreateRun) -> Result<Run, StoreError> {
        let expected_path = format!("{}/runs/{}", input.resource_path, input.request_id);
        if input.path != expected_path {
            return Err(StoreError::Invalid(format!(
                "Run path must be {expected_path}"
            )));
        }
        validate_object_path("Run path", &input.path)?;
        let existing = self
            .connection
            .query_row(
                RUN_SELECT_BY_REQUEST,
                [input.request_id.to_string()],
                run_from_row,
            )
            .optional()?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let (driver_path, actions_json): (String, String) = self
            .connection
            .query_row(
                "SELECT d.id,m.actions_json FROM resources r
                 JOIN manifests m ON m.id=r.manifest_path
                 JOIN drivers d ON d.manifest_path=m.id WHERE r.id=?",
                [input.resource_path.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Resource {}", input.resource_path)))?;
        let actions: Vec<Action> = serde_json::from_str(&actions_json)?;
        let action = actions
            .iter()
            .find(|action| action.name == input.action)
            .ok_or_else(|| {
                StoreError::Invalid(format!(
                    "Action {} is not declared by the Manifest",
                    input.action
                ))
            })?;
        let validator = jsonschema::validator_for(&action.input_schema).map_err(|error| {
            StoreError::Invalid(format!(
                "Action {} has an invalid input schema: {error}",
                action.name
            ))
        })?;
        if let Err(error) = validator.validate(&input.input) {
            return Err(StoreError::Invalid(format!(
                "Input for Action {} does not match its schema: {error}",
                action.name
            )));
        }
        let now = Utc::now();
        let path = input.path.clone();
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO runs(id,request_id,resource_path,driver_path,driver_generation,action,input_json,status,output_json,error,created_at,started_at,finished_at)
             VALUES (?,?,?,?,NULL,?,?,'queued',NULL,NULL,?,NULL,NULL)",
            params![path, input.request_id.to_string(), input.resource_path, driver_path, input.action, serde_json::to_string(&input.input)?, stamp(now)],
        )?;
        let run = tx.query_row(RUN_SELECT_BY_ID, [&path], run_from_row)?;
        let manifest_path = manifest_path_for_run(&tx, &path)?;
        append_lifecycle_event(
            &tx,
            EventType::Created,
            ObjectKind::Run,
            &path,
            Some(manifest_path),
            None,
            &run,
            now,
        )?;
        tx.commit()?;
        Ok(run)
    }

    pub fn claim_run(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Option<Run>, StoreError> {
        let tx = self.connection.transaction()?;
        let ready: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM drivers WHERE id=? AND generation=? AND state='ready')",
            params![driver_path.to_string(), generation],
            |row| row.get(0),
        )?;
        if !ready {
            return Err(StoreError::Conflict("Driver is stale or not ready".into()));
        }
        let run_path: Option<String> = tx
            .query_row(
                "SELECT id FROM runs WHERE driver_path=? AND status='queued' ORDER BY created_at,id LIMIT 1",
                [driver_path.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(run_path) = run_path else {
            tx.commit()?;
            return Ok(None);
        };
        let now = Utc::now();
        tx.execute(
            "UPDATE runs SET status='running',driver_generation=?,started_at=? WHERE id=? AND status='queued'",
            params![generation, stamp(now), run_path],
        )?;
        let run = tx.query_row(RUN_SELECT_BY_ID, [&run_path], run_from_row)?;
        let manifest_path = manifest_path_for_run(&tx, &run_path)?;
        append_lifecycle_event(
            &tx,
            EventType::Updated,
            ObjectKind::Run,
            &run_path,
            Some(manifest_path),
            None,
            &run,
            now,
        )?;
        tx.commit()?;
        Ok(Some(run))
    }

    pub fn claim_driver_work(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Option<DriverWork>, StoreError> {
        if let Some(resource) = self.claim_reconciliation(driver_path, generation)? {
            return Ok(Some(DriverWork::Reconcile {
                revision: resource.revision,
                resource,
            }));
        }
        let Some(run) = self.claim_run(driver_path, generation)? else {
            return Ok(None);
        };
        let resource = self.get_resource(&run.resource_path)?;
        Ok(Some(DriverWork::Run {
            run: Box::new(run),
            resource,
        }))
    }

    fn claim_reconciliation(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Option<Resource>, StoreError> {
        let tx = self.connection.transaction()?;
        let manifest_path: Option<String> = tx
            .query_row(
                "SELECT manifest_path FROM drivers WHERE id=? AND generation=? AND state='ready'",
                params![driver_path.to_string(), generation],
                |row| row.get(0),
            )
            .optional()?;
        let Some(manifest_path) = manifest_path else {
            return Err(StoreError::Conflict("Driver is stale or not ready".into()));
        };
        let resource: Option<(String, u64)> = tx
            .query_row(
                 "SELECT id,revision FROM resources
                 WHERE manifest_path=? AND observed_revision < revision
                 AND (claimed_revision IS NULL OR claimed_revision != revision OR claim_driver_generation != ?)
                 ORDER BY created_at,id LIMIT 1",
                params![manifest_path, generation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((resource_path, revision)) = resource else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "UPDATE resources SET claimed_revision=?,claim_driver_generation=? WHERE id=?",
            params![revision, generation, resource_path],
        )?;
        tx.commit()?;
        Ok(Some(self.get_resource(&resource_path)?))
    }

    pub fn finish_run(&mut self, run_path: &str, input: FinishRun) -> Result<Run, StoreError> {
        self.finish_run_with_mutations(run_path, input, Vec::new())
    }

    pub fn finish_run_with_mutations(
        &mut self,
        run_path: &str,
        input: FinishRun,
        mutations: Vec<Mutation>,
    ) -> Result<Run, StoreError> {
        self.finish_run_internal(run_path, input, mutations, None)
    }

    pub fn finish_run_delivery_with_mutations(
        &mut self,
        delivery_id: Uuid,
        driver_path: &str,
        generation: u64,
        run_path: &str,
        input: FinishRun,
        mutations: Vec<Mutation>,
    ) -> Result<Run, StoreError> {
        self.finish_run_internal(
            run_path,
            input,
            mutations,
            Some((delivery_id, driver_path.to_string(), generation)),
        )
    }

    fn finish_run_internal(
        &mut self,
        run_path: &str,
        input: FinishRun,
        mutations: Vec<Mutation>,
        delivery: Option<(Uuid, String, u64)>,
    ) -> Result<Run, StoreError> {
        let existing = self.get_run(run_path)?;
        if existing.status != RunStatus::Running {
            let same_result = existing.driver_generation == Some(input.driver_generation)
                && match &input.result {
                    RunResult::Succeeded { output } => {
                        existing.status == RunStatus::Succeeded
                            && existing.output.as_ref() == Some(output)
                            && existing.error.is_none()
                    }
                    RunResult::Failed { error } => {
                        existing.status == RunStatus::Failed
                            && existing.error.as_ref() == Some(error)
                            && existing.output.is_none()
                    }
                };
            if same_result {
                if let Some((delivery_id, driver_path, generation)) = delivery {
                    self.complete_driver_delivery(delivery_id, &driver_path, generation)?;
                }
                return Ok(existing);
            }
            return Err(StoreError::Conflict(
                "Run already has a different result".into(),
            ));
        }
        if matches!(&input.result, RunResult::Failed { .. }) && !mutations.is_empty() {
            return Err(StoreError::Invalid(
                "A failed Run cannot commit mutations".into(),
            ));
        }
        let tx = self.connection.transaction()?;
        apply_mutations(&tx, &mutations)?;
        let now = Utc::now();
        let (status, output, error) = match input.result {
            RunResult::Succeeded { output } => {
                ("succeeded", Some(serde_json::to_string(&output)?), None)
            }
            RunResult::Failed { error } => ("failed", None, Some(error)),
        };
        let changed = tx.execute(
            "UPDATE runs SET status=?,output_json=?,error=?,finished_at=?
             WHERE id=? AND status='running' AND driver_generation=?
             AND EXISTS(SELECT 1 FROM drivers d WHERE d.id=runs.driver_path AND d.generation=? AND d.state='ready')",
            params![status, output, error, stamp(now), run_path.to_string(), input.driver_generation, input.driver_generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Run is not owned by the current Driver generation".into(),
            ));
        }
        let run = tx.query_row(RUN_SELECT_BY_ID, [run_path.to_string()], run_from_row)?;
        append_lifecycle_event(
            &tx,
            EventType::Updated,
            ObjectKind::Run,
            run_path,
            Some(manifest_path_for_run(&tx, run_path)?),
            None,
            &run,
            now,
        )?;
        if let Some((delivery_id, driver_path, generation)) = delivery {
            complete_delivery_in_tx(&tx, delivery_id, &driver_path, generation)?;
        }
        tx.commit()?;
        Ok(run)
    }

    pub fn get_run(&self, run_path: &str) -> Result<Run, StoreError> {
        self.connection
            .query_row(RUN_SELECT_BY_ID, [run_path.to_string()], run_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Run {run_path}")))
    }

    pub fn create_link(&mut self, input: CreateLink) -> Result<Link, StoreError> {
        validate_object_path("Link path", &input.path)?;
        validate_name("Link relation", &input.relation)?;
        ensure_object_exists(&self.connection, &input.source)?;
        ensure_object_exists(&self.connection, &input.target)?;
        let path = input.path.clone();
        let now = Utc::now();
        let tx = self.connection.transaction()?;
        tx.execute(
                "INSERT INTO links(id,source_kind,source_path,relation,target_kind,target_path,metadata_json,created_at)
                 VALUES (?,?,?,?,?,?,?,?)",
                params![
                    path,
                    object_kind(&input.source.kind),
                    input.source.path.to_string(),
                    input.relation,
                    object_kind(&input.target.kind),
                    input.target.path.to_string(),
                    serde_json::to_string(&input.metadata)?,
                    stamp(now)
                ],
            )
            .map_err(|error| constraint(error, "Link already exists"))?;
        let link = Link {
            path: path.clone(),
            source: input.source,
            relation: input.relation,
            target: input.target,
            metadata: input.metadata,
            created_at: now,
        };
        append_lifecycle_event(
            &tx,
            EventType::Created,
            ObjectKind::Link,
            &path,
            object_manifest_path(&tx, &link.source)?,
            None,
            &link,
            now,
        )?;
        tx.commit()?;
        Ok(link)
    }

    pub fn get_link(&self, path: &str) -> Result<Link, StoreError> {
        self.connection
            .query_row(LINK_SELECT_BY_ID, [path], link_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Link {path}")))
    }

    pub fn list_links(&self, filter: LinkFilter) -> Result<Vec<Link>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,source_kind,source_path,relation,target_kind,target_path,metadata_json,created_at
             FROM links ORDER BY created_at,id",
        )?;
        let rows = statement.query_map([], link_from_row)?;
        let links = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(links
            .into_iter()
            .filter(|link| {
                filter
                    .source
                    .as_ref()
                    .is_none_or(|value| value == &link.source)
                    && filter
                        .relation
                        .as_ref()
                        .is_none_or(|value| value == &link.relation)
                    && filter
                        .target
                        .as_ref()
                        .is_none_or(|value| value == &link.target)
            })
            .collect())
    }

    pub fn delete_link(&mut self, path: &str) -> Result<(), StoreError> {
        let tx = self.connection.transaction()?;
        let link = tx
            .query_row(LINK_SELECT_BY_ID, [path], link_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Link {path}")))?;
        if tx.execute("DELETE FROM links WHERE id=?", [path])? != 1 {
            return Err(StoreError::NotFound(format!("Link {path}")));
        }
        append_lifecycle_event(
            &tx,
            EventType::Deleted,
            ObjectKind::Link,
            path,
            object_manifest_path(&tx, &link.source)?,
            None,
            &link,
            Utc::now(),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_events(
        &self,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Event>, StoreError> {
        self.list_events_filtered(EventFilter {
            after_sequence,
            limit: Some(limit),
            ..Default::default()
        })
    }

    pub fn current_event_cursor(&self) -> Result<u64, StoreError> {
        self.connection
            .query_row("SELECT COALESCE(MAX(sequence),0) FROM events", [], |row| {
                row.get(0)
            })
            .map_err(StoreError::from)
    }

    pub fn list_events_filtered(&self, filter: EventFilter) -> Result<Vec<Event>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT sequence,event_type,object_kind,object_path,manifest_path,revision,value_json,created_at
             FROM events WHERE (?1 IS NULL OR object_kind=?1)
             AND (?2 IS NULL OR object_path=?2) AND (?3 IS NULL OR manifest_path=?3)
             AND sequence>?4 ORDER BY sequence LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![
                filter.object_kind.as_ref().map(object_kind),
                filter.object_path.map(|id| id.to_string()),
                filter.manifest_path.map(|id| id.to_string()),
                filter.after_sequence.unwrap_or(0),
                filter.limit.unwrap_or(100).clamp(1, 1000)
            ],
            event_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn pending_driver_deliveries(
        &self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Vec<DriverDelivery>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,driver_path,generation,work_json,status,created_at,acked_at,completed_at
             FROM driver_deliveries WHERE driver_path=? AND generation=? AND status!='completed'
             ORDER BY created_at,id",
        )?;
        let rows = statement.query_map(
            params![driver_path.to_string(), generation],
            delivery_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn claim_driver_delivery(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Option<DriverDelivery>, StoreError> {
        if let Some(delivery) = self
            .pending_driver_deliveries(driver_path, generation)?
            .into_iter()
            .next()
        {
            return Ok(Some(delivery));
        }
        let Some(work) = self.claim_driver_work(driver_path, generation)? else {
            return Ok(None);
        };
        let id = Uuid::new_v4();
        let now = Utc::now();
        self.connection.execute(
            "INSERT INTO driver_deliveries(id,driver_path,generation,work_json,status,created_at)
             VALUES (?,?,?,?,'pending',?)",
            params![
                id.to_string(),
                driver_path.to_string(),
                generation,
                serde_json::to_string(&work)?,
                stamp(now)
            ],
        )?;
        Ok(Some(DriverDelivery {
            id,
            driver_path: driver_path.to_string(),
            generation,
            work,
            status: DeliveryStatus::Pending,
            created_at: now,
            acked_at: None,
            completed_at: None,
        }))
    }

    pub fn acknowledge_driver_delivery(
        &mut self,
        id: Uuid,
        driver_path: &str,
        generation: u64,
    ) -> Result<DriverDelivery, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE driver_deliveries SET status='acked',acked_at=?
             WHERE id=? AND driver_path=? AND generation=? AND status='pending'",
            params![
                stamp(now),
                id.to_string(),
                driver_path.to_string(),
                generation
            ],
        )?;
        if changed == 0 {
            let existing = self.get_driver_delivery(id)?;
            if existing.driver_path == driver_path
                && existing.generation == generation
                && existing.status == DeliveryStatus::Acked
            {
                return Ok(existing);
            }
            return Err(StoreError::Conflict(
                "Delivery is stale or already completed".into(),
            ));
        }
        self.get_driver_delivery(id)
    }

    pub fn complete_driver_delivery(
        &mut self,
        id: Uuid,
        driver_path: &str,
        generation: u64,
    ) -> Result<DriverDelivery, StoreError> {
        let changed = self.connection.execute(
            "UPDATE driver_deliveries SET status='completed',completed_at=?
             WHERE id=? AND driver_path=? AND generation=? AND status!='completed'",
            params![
                stamp(Utc::now()),
                id.to_string(),
                driver_path.to_string(),
                generation
            ],
        )?;
        if changed == 0 {
            let existing = self.get_driver_delivery(id)?;
            if existing.driver_path == driver_path
                && existing.generation == generation
                && existing.status == DeliveryStatus::Completed
            {
                return Ok(existing);
            }
            return Err(StoreError::Conflict("Delivery is stale".into()));
        }
        self.get_driver_delivery(id)
    }

    pub fn get_driver_delivery(&self, id: Uuid) -> Result<DriverDelivery, StoreError> {
        self.connection
            .query_row(DELIVERY_SELECT_BY_ID, [id.to_string()], delivery_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Driver delivery {id}")))
    }

    pub fn bootstrap_admin(&mut self, name: &str) -> Result<IssuedCredential, StoreError> {
        validate_name("User name", name)?;
        let existing: bool =
            self.connection
                .query_row("SELECT EXISTS(SELECT 1 FROM users)", [], |row| row.get(0))?;
        if existing {
            return Err(StoreError::Conflict(
                "An administrator is already bootstrapped".into(),
            ));
        }
        let now = Utc::now();
        let user_path = format!("/users/{name}");
        validate_object_path("Bootstrap User path", &user_path)?;
        let binding_path = "/role-bindings/system/bootstrap-admin".to_string();
        let token = kas_auth::issue_token();
        let credential_path = format!("{user_path}/credentials/{}", Uuid::new_v4());
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO users(id,name,disabled,created_at) VALUES (?,?,0,?)",
            params![user_path.to_string(), name, stamp(now)],
        )?;
        tx.execute(
            "INSERT INTO role_bindings(id,name,role_path,managed_by,created_at) VALUES (?,?,?,'system',?)",
            params![binding_path.to_string(), "system:bootstrap-admin", SYSTEM_ADMIN_ROLE, stamp(now)],
        )?;
        tx.execute(
            "INSERT INTO role_binding_subjects(role_binding_path,subject_kind,subject_path) VALUES (?,'user',?)",
            params![binding_path.to_string(), user_path.to_string()],
        )?;
        tx.execute(
            "INSERT INTO credentials(id,subject_kind,subject_path,token_hash,driver_generation,expires_at,revoked_at,created_at)
             VALUES (?,'user',?,?,NULL,NULL,NULL,?)",
            params![credential_path.to_string(), user_path.to_string(), kas_auth::token_hash(&token), stamp(now)],
        )?;
        tx.commit()?;
        Ok(IssuedCredential {
            path: credential_path,
            token,
            expires_at: None,
        })
    }

    pub fn authenticate(&self, token: &str) -> Result<AuthContext, StoreError> {
        let hash = kas_auth::token_hash(token);
        let now = stamp(Utc::now());
        let row: Option<(String, String, Option<u64>, Option<String>)> = self.connection
            .query_row(
                "SELECT c.subject_kind,c.subject_path,c.driver_generation,sa.driver_path
                 FROM credentials c
                 LEFT JOIN users u ON c.subject_kind='user' AND u.id=c.subject_path
                 LEFT JOIN service_accounts sa ON c.subject_kind='service_account' AND sa.id=c.subject_path
                 WHERE c.token_hash=? AND c.revoked_at IS NULL
                 AND (c.expires_at IS NULL OR c.expires_at>?)
                 AND ((c.subject_kind='user' AND u.disabled=0) OR (c.subject_kind='service_account' AND sa.id IS NOT NULL))",
                params![hash, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((kind, subject_path, driver_generation, driver_path)) = row else {
            return Err(StoreError::Invalid("Invalid or expired credential".into()));
        };
        let kind = parse_subject_kind(&kind)?;
        if let (Some(driver_path), Some(generation)) = (&driver_path, driver_generation) {
            let driver = self.get_driver(driver_path)?;
            if driver.generation != generation {
                return Err(StoreError::Invalid(
                    "Driver credential generation is stale".into(),
                ));
            }
        }
        let mut statement = self.connection.prepare(
            "SELECT r.rules_json FROM roles r
             JOIN role_bindings rb ON rb.role_path=r.id
             JOIN role_binding_subjects rbs ON rbs.role_binding_path=rb.id
             WHERE rbs.subject_kind=? AND rbs.subject_path=?",
        )?;
        let rows = statement
            .query_map(params![kind.as_str(), subject_path.to_string()], |row| {
                row.get::<_, String>(0)
            })?;
        let mut rules = Vec::new();
        for row in rows {
            rules.extend(serde_json::from_str::<Vec<Rule>>(&row?)?);
        }
        Ok(AuthContext {
            subject: Subject {
                kind,
                path: subject_path,
            },
            rules,
            driver_path,
            driver_generation,
        })
    }

    pub fn issue_driver_credential(
        &mut self,
        driver_path: &str,
    ) -> Result<IssuedCredential, StoreError> {
        let driver = self.get_driver(driver_path)?;
        if driver.state != DriverState::Starting {
            return Err(StoreError::Conflict(
                "Driver must be starting before credentials are issued".into(),
            ));
        }
        let service_account_path: String = self.connection.query_row(
            "SELECT id FROM service_accounts WHERE driver_path=? AND managed_by='system'",
            [driver_path.to_string()],
            |row| row.get(0),
        )?;
        self.issue_credential(
            Subject {
                kind: SubjectKind::ServiceAccount,
                path: service_account_path,
            },
            Some(driver.generation),
            Some(Utc::now() + chrono::Duration::hours(1)),
            true,
        )
    }

    pub fn issue_user_credential(
        &mut self,
        user_path: &str,
    ) -> Result<IssuedCredential, StoreError> {
        self.get_user(user_path)?;
        self.issue_credential(
            Subject {
                kind: SubjectKind::User,
                path: user_path.to_string(),
            },
            None,
            None,
            false,
        )
    }

    pub fn issue_service_account_credential(
        &mut self,
        service_account_path: &str,
    ) -> Result<IssuedCredential, StoreError> {
        let driver_path: Option<String> = self
            .connection
            .query_row(
                "SELECT driver_path FROM service_accounts WHERE id=?",
                [service_account_path.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::NotFound(format!("ServiceAccount {service_account_path}"))
            })?;
        if driver_path.is_some() {
            return Err(StoreError::Conflict(
                "Driver credentials must be issued through the Driver endpoint".into(),
            ));
        }
        self.issue_credential(
            Subject {
                kind: SubjectKind::ServiceAccount,
                path: service_account_path.to_string(),
            },
            None,
            None,
            false,
        )
    }

    fn issue_credential(
        &mut self,
        subject: Subject,
        driver_generation: Option<u64>,
        expires_at: Option<DateTime<Utc>>,
        revoke_existing: bool,
    ) -> Result<IssuedCredential, StoreError> {
        let now = Utc::now();
        let path = format!("{}/credentials/{}", subject.path, Uuid::new_v4());
        let token = kas_auth::issue_token();
        let tx = self.connection.transaction()?;
        if revoke_existing {
            tx.execute(
                "UPDATE credentials SET revoked_at=? WHERE subject_kind=? AND subject_path=? AND revoked_at IS NULL",
                params![stamp(now), subject.kind.as_str(), subject.path.to_string()],
            )?;
        }
        tx.execute(
            "INSERT INTO credentials(id,subject_kind,subject_path,token_hash,driver_generation,expires_at,revoked_at,created_at)
             VALUES (?,?,?,?,?,?,NULL,?)",
            params![path, subject.kind.as_str(), subject.path, kas_auth::token_hash(&token), driver_generation, expires_at.map(stamp), stamp(now)],
        )?;
        tx.commit()?;
        Ok(IssuedCredential {
            path,
            token,
            expires_at,
        })
    }

    pub fn create_user(&mut self, input: CreateUser) -> Result<User, StoreError> {
        validate_name("User name", &input.name)?;
        validate_object_path("User path", &input.path)?;
        let now = Utc::now();
        self.connection
            .execute(
                "INSERT INTO users(id,name,disabled,created_at) VALUES (?,?,0,?)",
                params![input.path, input.name, stamp(now)],
            )
            .map_err(|error| constraint(error, "User name already exists"))?;
        Ok(User {
            path: input.path,
            name: input.name,
            disabled: false,
            created_at: now,
        })
    }

    pub fn get_user(&self, path: &str) -> Result<User, StoreError> {
        self.connection
            .query_row(
                "SELECT id,name,disabled,created_at FROM users WHERE id=?",
                [path],
                user_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("User {path}")))
    }

    pub fn list_users(&self) -> Result<Vec<User>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT id,name,disabled,created_at FROM users ORDER BY name")?;
        let rows = statement.query_map([], user_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn create_service_account(
        &mut self,
        input: CreateServiceAccount,
    ) -> Result<ServiceAccount, StoreError> {
        validate_name("ServiceAccount name", &input.name)?;
        validate_object_path("ServiceAccount path", &input.path)?;
        let now = Utc::now();
        self.connection.execute(
            "INSERT INTO service_accounts(id,name,driver_path,managed_by,created_at) VALUES (?,?,NULL,'user',?)",
            params![input.path, input.name, stamp(now)],
        ).map_err(|error| constraint(error, "ServiceAccount name already exists"))?;
        Ok(ServiceAccount {
            path: input.path,
            name: input.name,
            driver_path: None,
            managed_by: "user".into(),
            created_at: now,
        })
    }

    pub fn list_service_accounts(&self) -> Result<Vec<ServiceAccount>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,name,driver_path,managed_by,created_at FROM service_accounts ORDER BY name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ServiceAccount {
                path: row.get(0)?,
                name: row.get(1)?,
                driver_path: row.get(2)?,
                managed_by: row.get(3)?,
                created_at: time_from_row(row, 4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_service_account(&self, path: &str) -> Result<ServiceAccount, StoreError> {
        self.connection
            .query_row(
                "SELECT id,name,driver_path,managed_by,created_at
                 FROM service_accounts WHERE id=?",
                [path],
                |row| {
                    Ok(ServiceAccount {
                        path: row.get(0)?,
                        name: row.get(1)?,
                        driver_path: row.get(2)?,
                        managed_by: row.get(3)?,
                        created_at: time_from_row(row, 4)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("ServiceAccount {path}")))
    }

    pub fn create_role(&mut self, input: CreateRole) -> Result<Role, StoreError> {
        validate_name("Role name", &input.name)?;
        validate_object_path("Role path", &input.path)?;
        validate_rules(&input.rules)?;
        let now = Utc::now();
        self.connection.execute(
            "INSERT INTO roles(id,name,description,rules_json,managed_by,created_at,updated_at) VALUES (?,?,?,?,'user',?,?)",
            params![input.path, input.name, input.description, serde_json::to_string(&input.rules)?, stamp(now), stamp(now)],
        ).map_err(|error| constraint(error, "Role name already exists"))?;
        Ok(Role {
            path: input.path,
            name: input.name,
            description: input.description,
            rules: input.rules,
            managed_by: "user".into(),
            created_at: now,
            updated_at: now,
        })
    }

    pub fn list_roles(&self) -> Result<Vec<Role>, StoreError> {
        let mut statement = self.connection.prepare("SELECT id,name,description,rules_json,managed_by,created_at,updated_at FROM roles ORDER BY name")?;
        let rows = statement.query_map([], role_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_role(&self, path: &str) -> Result<Role, StoreError> {
        self.connection
            .query_row(
                "SELECT id,name,description,rules_json,managed_by,created_at,updated_at
                 FROM roles WHERE id=?",
                [path],
                role_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Role {path}")))
    }

    pub fn update_role(&mut self, path: &str, input: CreateRole) -> Result<Role, StoreError> {
        validate_name("Role name", &input.name)?;
        validate_rules(&input.rules)?;
        if input.path != path {
            return Err(StoreError::Invalid("Role path is immutable".into()));
        }
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE roles SET name=?,description=?,rules_json=?,updated_at=? WHERE id=? AND managed_by='user'",
            params![input.name, input.description, serde_json::to_string(&input.rules)?, stamp(now), path],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "System Role cannot be modified".into(),
            ));
        }
        self.connection.query_row(
            "SELECT id,name,description,rules_json,managed_by,created_at,updated_at FROM roles WHERE id=?",
            [path], role_from_row,
        ).map_err(StoreError::from)
    }

    pub fn delete_role(&mut self, path: &str) -> Result<(), StoreError> {
        let changed = self
            .connection
            .execute("DELETE FROM roles WHERE id=? AND managed_by='user'", [path])
            .map_err(|error| constraint(error, "Role is still bound"))?;
        if changed != 1 {
            return Err(StoreError::Conflict("System Role cannot be deleted".into()));
        }
        Ok(())
    }

    pub fn create_role_binding(
        &mut self,
        input: CreateRoleBinding,
    ) -> Result<RoleBinding, StoreError> {
        validate_name("RoleBinding name", &input.name)?;
        validate_object_path("Referenced Role path", &input.role_path)?;
        if input.subjects.is_empty() {
            return Err(StoreError::Invalid("RoleBinding requires a subject".into()));
        }
        validate_object_path("RoleBinding path", &input.path)?;
        let path = input.path.clone();
        let now = Utc::now();
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO role_bindings(id,name,role_path,managed_by,created_at) VALUES (?,?,?,'user',?)",
            params![path, input.name, input.role_path, stamp(now)],
        ).map_err(|error| constraint(error, "Role or RoleBinding is invalid"))?;
        for subject in &input.subjects {
            ensure_subject_exists(&tx, subject)?;
            tx.execute(
                "INSERT INTO role_binding_subjects(role_binding_path,subject_kind,subject_path) VALUES (?,?,?)",
                params![path, subject.kind.as_str(), subject.path],
            )?;
        }
        tx.commit()?;
        Ok(RoleBinding {
            path,
            name: input.name,
            role_path: input.role_path,
            subjects: input.subjects,
            managed_by: "user".into(),
            created_at: now,
        })
    }

    pub fn list_role_bindings(&self) -> Result<Vec<RoleBinding>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,name,role_path,managed_by,created_at FROM role_bindings ORDER BY name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                time_from_row(row, 4)?,
            ))
        })?;
        let base = rows.collect::<Result<Vec<_>, _>>()?;
        let mut bindings = Vec::new();
        for (path, name, role_path, managed_by, created_at) in base {
            let mut subjects_statement = self.connection.prepare(
                "SELECT subject_kind,subject_path FROM role_binding_subjects WHERE role_binding_path=? ORDER BY subject_kind,subject_path",
            )?;
            let subjects = subjects_statement
                .query_map([path.as_str()], |row| {
                    let kind: String = row.get(0)?;
                    let kind = match kind.as_str() {
                        "user" => SubjectKind::User,
                        "service_account" => SubjectKind::ServiceAccount,
                        other => return Err(from_sql(0, format!("invalid subject kind {other}"))),
                    };
                    Ok(Subject {
                        kind,
                        path: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            bindings.push(RoleBinding {
                path,
                name,
                role_path,
                subjects,
                managed_by,
                created_at,
            });
        }
        Ok(bindings)
    }

    pub fn delete_role_binding(&mut self, path: &str) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "DELETE FROM role_bindings WHERE id=? AND managed_by='user'",
            [path],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "System RoleBinding cannot be deleted".into(),
            ));
        }
        Ok(())
    }
}

fn apply_mutations(tx: &Transaction<'_>, mutations: &[Mutation]) -> Result<(), StoreError> {
    for mutation in mutations {
        match mutation {
            Mutation::CreateResource { resource } => {
                validate_name("Resource name", &resource.name)?;
                validate_object_path("Mutation Resource path", &resource.path)?;
                let schema: String = tx
                    .query_row(
                        "SELECT resource_schema_json FROM manifests WHERE id=?",
                        [resource.manifest_path.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        StoreError::NotFound(format!("Manifest {}", resource.manifest_path))
                    })?;
                validate_json_schema(
                    "Resource spec",
                    &serde_json::from_str::<Value>(&schema)?,
                    &resource.spec,
                )?;
                let now = Utc::now();
                tx.execute(
                    "INSERT INTO resources(id,manifest_path,name,spec_json,status_json,revision,created_at,updated_at)
                     VALUES (?,?,?,?,'{}',0,?,?)",
                    params![
                        resource.path.to_string(),
                        resource.manifest_path.to_string(),
                        resource.name,
                        serde_json::to_string(&resource.spec)?,
                        stamp(now),
                        stamp(now)
                    ],
                )
                .map_err(|error| constraint(error, "Mutation Resource already exists"))?;
                let created = tx.query_row(
                    RESOURCE_SELECT_BY_ID,
                    [resource.path.to_string()],
                    resource_from_row,
                )?;
                append_lifecycle_event(
                    tx,
                    EventType::Created,
                    ObjectKind::Resource,
                    &resource.path,
                    Some(resource.manifest_path.clone()),
                    Some(0),
                    &created,
                    now,
                )?;
            }
            Mutation::UpdateResource {
                resource_path,
                expected_revision,
                spec,
            } => {
                let schema: String = tx
                    .query_row(
                        "SELECT m.resource_schema_json FROM resources r JOIN manifests m ON m.id=r.manifest_path WHERE r.id=?",
                        [resource_path.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or_else(|| StoreError::NotFound(format!("Resource {resource_path}")))?;
                validate_json_schema(
                    "Resource spec",
                    &serde_json::from_str::<Value>(&schema)?,
                    spec,
                )?;
                if tx.execute(
                    "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=? WHERE id=? AND revision=?",
                    params![
                        serde_json::to_string(spec)?,
                        stamp(Utc::now()),
                        resource_path.to_string(),
                        expected_revision
                    ],
                )? != 1
                {
                    return Err(StoreError::Conflict(format!(
                        "Resource {resource_path} revision is stale"
                    )));
                }
                let updated = tx.query_row(
                    RESOURCE_SELECT_BY_ID,
                    [resource_path.to_string()],
                    resource_from_row,
                )?;
                append_lifecycle_event(
                    tx,
                    EventType::Updated,
                    ObjectKind::Resource,
                    resource_path,
                    Some(updated.manifest_path.clone()),
                    Some(updated.revision),
                    &updated,
                    updated.updated_at,
                )?;
            }
            Mutation::CreateLink { link } => {
                validate_name("Link relation", &link.relation)?;
                validate_object_path("Mutation Link path", &link.path)?;
                ensure_object_exists(tx, &link.source)?;
                ensure_object_exists(tx, &link.target)?;
                let now = Utc::now();
                tx.execute(
                    "INSERT INTO links(id,source_kind,source_path,relation,target_kind,target_path,metadata_json,created_at)
                     VALUES (?,?,?,?,?,?,?,?)",
                    params![
                        link.path.to_string(),
                        object_kind(&link.source.kind),
                        link.source.path.to_string(),
                        link.relation,
                        object_kind(&link.target.kind),
                        link.target.path.to_string(),
                        serde_json::to_string(&link.metadata)?,
                        stamp(now)
                    ],
                )
                .map_err(|error| constraint(error, "Mutation Link already exists"))?;
                let created =
                    tx.query_row(LINK_SELECT_BY_ID, [link.path.to_string()], link_from_row)?;
                append_lifecycle_event(
                    tx,
                    EventType::Created,
                    ObjectKind::Link,
                    &link.path,
                    object_manifest_path(tx, &link.source)?,
                    None,
                    &created,
                    now,
                )?;
            }
            Mutation::UpdateResourceStatus { .. } | Mutation::CompleteRun { .. } => {
                return Err(StoreError::Invalid(
                    "Lifecycle operations must be applied through a Driver delivery".into(),
                ));
            }
        }
    }
    Ok(())
}

fn complete_delivery_in_tx(
    tx: &Transaction<'_>,
    id: Uuid,
    driver_path: &str,
    generation: u64,
) -> Result<(), StoreError> {
    let changed = tx.execute(
        "UPDATE driver_deliveries SET status='completed',completed_at=?
         WHERE id=? AND driver_path=? AND generation=? AND status!='completed'",
        params![
            stamp(Utc::now()),
            id.to_string(),
            driver_path.to_string(),
            generation
        ],
    )?;
    if changed == 1 {
        return Ok(());
    }
    let existing: Option<(String, u64, String)> = tx
        .query_row(
            "SELECT driver_path,generation,status FROM driver_deliveries WHERE id=?",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if matches!(
        existing,
        Some((ref existing_driver, existing_generation, ref status))
            if existing_driver == &driver_path.to_string()
                && existing_generation == generation
                && status == "completed"
    ) {
        return Ok(());
    }
    Err(StoreError::Conflict("Delivery is stale".into()))
}

#[allow(clippy::too_many_arguments)]
fn append_lifecycle_event(
    tx: &Transaction<'_>,
    event_type: EventType,
    object_kind_value: ObjectKind,
    object_path: &str,
    manifest_path: Option<String>,
    revision: Option<u64>,
    value: &impl Serialize,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO events(event_type,object_kind,object_path,manifest_path,revision,value_json,created_at)
         VALUES (?,?,?,?,?,?,?)",
        params![
            event_type_str(event_type),
            object_kind(&object_kind_value),
            object_path.to_string(),
            manifest_path.map(|id| id.to_string()),
            revision,
            serde_json::to_string(value)?,
            stamp(now)
        ],
    )?;
    Ok(())
}

const DRIVER_SELECT_BY_ID: &str = "SELECT id,manifest_path,name,state,generation,process_id,metadata_json,started_at,heartbeat_at,stopped_at,error,created_at,updated_at FROM drivers WHERE id=?";
const DRIVER_SELECT_BY_MANIFEST: &str = "SELECT id,manifest_path,name,state,generation,process_id,metadata_json,started_at,heartbeat_at,stopped_at,error,created_at,updated_at FROM drivers WHERE manifest_path=?";
const RESOURCE_SELECT_BY_ID: &str = "SELECT id,manifest_path,name,spec_json,status_json,revision,created_at,updated_at FROM resources WHERE id=?";
const RUN_SELECT_BY_ID: &str = "SELECT id,request_id,resource_path,driver_path,driver_generation,action,input_json,status,output_json,error,created_at,started_at,finished_at FROM runs WHERE id=?";
const RUN_SELECT_BY_REQUEST: &str = "SELECT id,request_id,resource_path,driver_path,driver_generation,action,input_json,status,output_json,error,created_at,started_at,finished_at FROM runs WHERE request_id=?";
const LINK_SELECT_BY_ID: &str = "SELECT id,source_kind,source_path,relation,target_kind,target_path,metadata_json,created_at FROM links WHERE id=?";
const DELIVERY_SELECT_BY_ID: &str = "SELECT id,driver_path,generation,work_json,status,created_at,acked_at,completed_at FROM driver_deliveries WHERE id=?";

fn manifest_from_row(row: &Row<'_>) -> rusqlite::Result<Manifest> {
    Ok(Manifest {
        path: row.get(0)?,
        name: row.get(1)?,
        version: row.get(2)?,
        description: row.get(3)?,
        resource_schema: json_from_row(row, 4)?,
        actions: json_from_row(row, 5)?,
        driver: row.get(6)?,
        created_at: time_from_row(row, 7)?,
    })
}

fn link_from_row(row: &Row<'_>) -> rusqlite::Result<Link> {
    Ok(Link {
        path: row.get(0)?,
        source: ObjectRef {
            kind: object_kind_from_str(&row.get::<_, String>(1)?, 1)?,
            path: row.get(2)?,
        },
        relation: row.get(3)?,
        target: ObjectRef {
            kind: object_kind_from_str(&row.get::<_, String>(4)?, 4)?,
            path: row.get(5)?,
        },
        metadata: json_from_row(row, 6)?,
        created_at: time_from_row(row, 7)?,
    })
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        sequence: row.get(0)?,
        event_type: event_type_from_str(&row.get::<_, String>(1)?, 1)?,
        object_kind: object_kind_from_str(&row.get::<_, String>(2)?, 2)?,
        object_path: row.get(3)?,
        manifest_path: row.get(4)?,
        revision: row.get(5)?,
        value: json_from_row(row, 6)?,
        created_at: time_from_row(row, 7)?,
    })
}

fn delivery_from_row(row: &Row<'_>) -> rusqlite::Result<DriverDelivery> {
    Ok(DriverDelivery {
        id: uuid_from_row(row, 0)?,
        driver_path: row.get(1)?,
        generation: row.get(2)?,
        work: json_from_row(row, 3)?,
        status: match row.get::<_, String>(4)?.as_str() {
            "pending" => DeliveryStatus::Pending,
            "acked" => DeliveryStatus::Acked,
            "completed" => DeliveryStatus::Completed,
            value => return Err(from_sql(4, format!("invalid delivery status {value}"))),
        },
        created_at: time_from_row(row, 5)?,
        acked_at: optional_time_from_row(row, 6)?,
        completed_at: optional_time_from_row(row, 7)?,
    })
}

fn resource_from_row(row: &Row<'_>) -> rusqlite::Result<Resource> {
    Ok(Resource {
        path: row.get(0)?,
        manifest_path: row.get(1)?,
        name: row.get(2)?,
        spec: json_from_row(row, 3)?,
        status: json_from_row(row, 4)?,
        revision: row.get(5)?,
        created_at: time_from_row(row, 6)?,
        updated_at: time_from_row(row, 7)?,
    })
}

fn user_from_row(row: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        path: row.get(0)?,
        name: row.get(1)?,
        disabled: row.get(2)?,
        created_at: time_from_row(row, 3)?,
    })
}

fn role_from_row(row: &Row<'_>) -> rusqlite::Result<Role> {
    Ok(Role {
        path: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        rules: json_from_row(row, 3)?,
        managed_by: row.get(4)?,
        created_at: time_from_row(row, 5)?,
        updated_at: time_from_row(row, 6)?,
    })
}

fn parse_subject_kind(value: &str) -> Result<SubjectKind, StoreError> {
    match value {
        "user" => Ok(SubjectKind::User),
        "service_account" => Ok(SubjectKind::ServiceAccount),
        other => Err(StoreError::Invalid(format!("Unknown subject kind {other}"))),
    }
}

fn ensure_subject_exists(tx: &Transaction<'_>, subject: &Subject) -> Result<(), StoreError> {
    validate_object_path("Subject path", &subject.path)?;
    let table = match subject.kind {
        SubjectKind::User => "users",
        SubjectKind::ServiceAccount => "service_accounts",
    };
    let exists: bool = tx.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?)"),
        [subject.path.to_string()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StoreError::NotFound(format!(
            "{} {}",
            subject.kind.as_str(),
            subject.path
        )));
    }
    Ok(())
}

fn driver_from_row(row: &Row<'_>) -> rusqlite::Result<Driver> {
    Ok(Driver {
        path: row.get(0)?,
        manifest_path: row.get(1)?,
        name: row.get(2)?,
        state: match row.get::<_, String>(3)?.as_str() {
            "stopped" => DriverState::Stopped,
            "starting" => DriverState::Starting,
            "ready" => DriverState::Ready,
            "stopping" => DriverState::Stopping,
            "failed" => DriverState::Failed,
            state => return Err(from_sql(3, format!("invalid Driver state {state}"))),
        },
        generation: row.get(4)?,
        process_id: row.get(5)?,
        metadata: json_from_row(row, 6)?,
        started_at: optional_time_from_row(row, 7)?,
        heartbeat_at: optional_time_from_row(row, 8)?,
        stopped_at: optional_time_from_row(row, 9)?,
        error: row.get(10)?,
        created_at: time_from_row(row, 11)?,
        updated_at: time_from_row(row, 12)?,
    })
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        path: row.get(0)?,
        request_id: uuid_from_row(row, 1)?,
        resource_path: row.get(2)?,
        driver_path: row.get(3)?,
        driver_generation: row.get(4)?,
        action: row.get(5)?,
        input: json_from_row(row, 6)?,
        status: match row.get::<_, String>(7)?.as_str() {
            "queued" => RunStatus::Queued,
            "running" => RunStatus::Running,
            "succeeded" => RunStatus::Succeeded,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            status => return Err(from_sql(7, format!("invalid Run status {status}"))),
        },
        output: optional_json_from_row(row, 8)?,
        error: row.get(9)?,
        created_at: time_from_row(row, 10)?,
        started_at: optional_time_from_row(row, 11)?,
        finished_at: optional_time_from_row(row, 12)?,
    })
}

fn driver_rules() -> Vec<Rule> {
    vec![
        Rule {
            resources: vec!["drivers".into()],
            verbs: vec!["get".into(), "patch".into()],
            paths: Vec::new(),
        },
        Rule {
            resources: vec!["drivers/connect".into(), "drivers/claim".into()],
            verbs: vec!["create".into()],
            paths: Vec::new(),
        },
        Rule {
            resources: vec!["resources/status".into(), "runs/result".into()],
            verbs: vec!["update".into()],
            paths: Vec::new(),
        },
    ]
}

fn validate_manifest_contract(resource_schema: &Value) -> Result<(), StoreError> {
    jsonschema::validator_for(resource_schema)
        .map_err(|error| StoreError::Invalid(format!("Resource schema is invalid: {error}")))?;
    Ok(())
}

fn validate_json_schema(label: &str, schema: &Value, instance: &Value) -> Result<(), StoreError> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| StoreError::Invalid(format!("{label} schema is invalid: {error}")))?;
    validator
        .validate(instance)
        .map_err(|error| StoreError::Invalid(format!("{label} does not match its schema: {error}")))
}

fn manifest_path_for_run(connection: &Connection, run_path: &str) -> Result<String, StoreError> {
    let value: String = connection.query_row(
        "SELECT r.manifest_path FROM runs ru JOIN resources r ON r.id=ru.resource_path WHERE ru.id=?",
        [run_path.to_string()],
        |row| row.get(0),
    )?;
    Ok(value)
}

fn object_manifest_path(
    connection: &Connection,
    object: &ObjectRef,
) -> Result<Option<String>, StoreError> {
    let value: Option<String> = match object.kind {
        ObjectKind::Resource => connection
            .query_row(
                "SELECT manifest_path FROM resources WHERE id=?",
                [object.path.to_string()],
                |row| row.get(0),
            )
            .optional()?,
        ObjectKind::Run => connection
            .query_row(
                "SELECT r.manifest_path FROM runs ru JOIN resources r ON r.id=ru.resource_path WHERE ru.id=?",
                [object.path.to_string()],
                |row| row.get(0),
            )
            .optional()?,
        _ => None,
    };
    Ok(value)
}

fn event_type_str(event_type: EventType) -> &'static str {
    match event_type {
        EventType::Created => "created",
        EventType::Updated => "updated",
        EventType::Deleted => "deleted",
    }
}

fn event_type_from_str(value: &str, index: usize) -> rusqlite::Result<EventType> {
    match value {
        "created" => Ok(EventType::Created),
        "updated" => Ok(EventType::Updated),
        "deleted" => Ok(EventType::Deleted),
        _ => Err(from_sql(index, format!("invalid Event type {value}"))),
    }
}

fn object_kind(kind: &ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Manifest => "manifest",
        ObjectKind::Resource => "resource",
        ObjectKind::Driver => "driver",
        ObjectKind::Run => "run",
        ObjectKind::Link => "link",
        ObjectKind::User => "user",
        ObjectKind::ServiceAccount => "service_account",
        ObjectKind::Role => "role",
        ObjectKind::RoleBinding => "role_binding",
        ObjectKind::Credential => "credential",
    }
}

fn object_kind_from_str(value: &str, index: usize) -> rusqlite::Result<ObjectKind> {
    match value {
        "manifest" => Ok(ObjectKind::Manifest),
        "resource" => Ok(ObjectKind::Resource),
        "driver" => Ok(ObjectKind::Driver),
        "run" => Ok(ObjectKind::Run),
        "link" => Ok(ObjectKind::Link),
        "user" => Ok(ObjectKind::User),
        "service_account" => Ok(ObjectKind::ServiceAccount),
        "role" => Ok(ObjectKind::Role),
        "role_binding" => Ok(ObjectKind::RoleBinding),
        "credential" => Ok(ObjectKind::Credential),
        _ => Err(from_sql(index, format!("invalid Object kind {value}"))),
    }
}

fn ensure_object_exists(connection: &Connection, object: &ObjectRef) -> Result<(), StoreError> {
    validate_object_path("Object reference path", &object.path)?;
    let table = match object.kind {
        ObjectKind::Manifest => "manifests",
        ObjectKind::Resource => "resources",
        ObjectKind::Driver => "drivers",
        ObjectKind::Run => "runs",
        ObjectKind::Link => "links",
        ObjectKind::User => "users",
        ObjectKind::ServiceAccount => "service_accounts",
        ObjectKind::Role => "roles",
        ObjectKind::RoleBinding => "role_bindings",
        ObjectKind::Credential => "credentials",
    };
    let exists: bool = connection.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?)"),
        [object.path.to_string()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StoreError::NotFound(format!(
            "{} {}",
            object_kind(&object.kind),
            object.path
        )));
    }
    Ok(())
}

fn validate_name(label: &str, value: &str) -> Result<(), StoreError> {
    if value.trim().is_empty() || value.chars().count() > 128 {
        return Err(StoreError::Invalid(format!("{label} is empty or too long")));
    }
    Ok(())
}

fn validate_permission_segment(label: &str, value: &str) -> Result<(), StoreError> {
    if value.contains('/') || value == "*" {
        return Err(StoreError::Invalid(format!(
            "{label} cannot contain / or equal *"
        )));
    }
    Ok(())
}

fn validate_object_path(label: &str, value: &str) -> Result<(), StoreError> {
    kas_auth::validate_path(value)
        .map_err(|error| StoreError::Invalid(format!("{label} is invalid: {error}")))
}

fn validate_rules(rules: &[Rule]) -> Result<(), StoreError> {
    for rule in rules {
        for path in &rule.paths {
            kas_auth::validate_path_pattern(path).map_err(|error| {
                StoreError::Invalid(format!("Rule path pattern {path} is invalid: {error}"))
            })?;
        }
    }
    Ok(())
}

fn stamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn uuid_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    parse_uuid(&row.get::<_, String>(index)?, index).map_err(|error| match error {
        StoreError::Database(error) => error,
        other => from_sql(index, other.to_string()),
    })
}

fn parse_uuid(value: &str, index: usize) -> Result<Uuid, StoreError> {
    Uuid::parse_str(value)
        .map_err(|error| StoreError::Database(from_sql(index, format!("invalid UUID: {error}"))))
}

fn json_from_row<T: serde::de::DeserializeOwned>(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<T> {
    serde_json::from_str(&row.get::<_, String>(index)?)
        .map_err(|error| from_sql(index, error.to_string()))
}

fn optional_json_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<Value>> {
    row.get::<_, Option<String>>(index)?
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| from_sql(index, error.to_string()))
        })
        .transpose()
}

fn time_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&row.get::<_, String>(index)?)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| from_sql(index, error.to_string()))
}

fn optional_time_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    row.get::<_, Option<String>>(index)?
        .map(|value| {
            DateTime::parse_from_rfc3339(&value)
                .map(|value| value.with_timezone(&Utc))
                .map_err(|error| from_sql(index, error.to_string()))
        })
        .transpose()
}

fn from_sql(index: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

fn constraint(error: rusqlite::Error, message: &str) -> StoreError {
    match error {
        rusqlite::Error::SqliteFailure(details, _)
            if details.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::Conflict(message.into())
        }
        other => StoreError::Database(other),
    }
}

fn configure(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch("PRAGMA foreign_keys=ON;")?;
    Ok(())
}

fn schema_version(connection: &Connection) -> Result<u32, StoreError> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn require_current_schema(connection: &Connection) -> Result<(), StoreError> {
    let current = schema_version(connection)?;
    if current < LATEST_SCHEMA_VERSION {
        return Err(StoreError::MigrationRequired {
            current,
            latest: LATEST_SCHEMA_VERSION,
        });
    }
    if current > LATEST_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            current,
            latest: LATEST_SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn migrate_connection(connection: &mut Connection) -> Result<u32, StoreError> {
    let mut current = schema_version(connection)?;
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
        if *version != current + 1 {
            return Err(StoreError::Invalid(format!(
                "missing migration from version {current} to {version}"
            )));
        }
        let tx = connection.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        current = *version;
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(path: &str) -> CreateManifest {
        CreateManifest {
            path: path.into(),
            name: "note".into(),
            version: 1,
            description: "notes".into(),
            resource_schema: json!({"type": "object"}),
            actions: Vec::new(),
            driver: None,
        }
    }

    #[test]
    fn migration_builds_path_schema() {
        let store = Store::memory().unwrap();
        assert_eq!(
            schema_version(&store.connection).unwrap(),
            LATEST_SCHEMA_VERSION
        );
        let columns: Vec<String> = {
            let mut statement = store
                .connection
                .prepare("PRAGMA table_info(events)")
                .unwrap();
            statement
                .query_map([], |row| row.get(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert!(columns.iter().any(|column| column == "object_path"));
        assert!(!columns.iter().any(|column| column == "object_id"));
        let editor = store.get_role("/roles/system/editor").unwrap();
        assert!(editor.rules[0].verbs.iter().any(|verb| verb == "link"));
    }

    #[test]
    fn objects_are_addressed_by_path_and_emit_path_events() {
        let mut store = Store::memory().unwrap();
        let created_manifest = store
            .create_manifest(manifest("/manifests/note/v1"))
            .unwrap();
        assert_eq!(created_manifest.path, "/manifests/note/v1");

        let resource = store
            .create_resource(CreateResource {
                path: "/notes/team-a/first".into(),
                manifest_path: created_manifest.path,
                name: "first".into(),
                spec: json!({"body": "hello"}),
            })
            .unwrap();
        assert_eq!(store.get_resource("/notes/team-a/first").unwrap(), resource);
        let events = store.list_events(Some(0), 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].object_path, resource.path);
    }

    #[test]
    fn invalid_object_paths_are_rejected() {
        let mut store = Store::memory().unwrap();
        let error = store
            .create_manifest(manifest("/manifests//note"))
            .unwrap_err();
        assert!(matches!(error, StoreError::Invalid(_)));
    }

    #[test]
    fn driver_identity_and_credentials_use_paths() {
        let mut store = Store::memory().unwrap();
        let mut input = manifest("/manifests/note/v1");
        input.driver = Some("note-driver".into());
        store.create_manifest(input).unwrap();

        let driver = store
            .driver_for_manifest("/manifests/note/v1")
            .unwrap()
            .unwrap();
        assert_eq!(driver.path, "/drivers/note");
        let driver = store.start_driver(&driver.path).unwrap();
        let credential = store.issue_driver_credential(&driver.path).unwrap();
        assert!(credential
            .path
            .starts_with("/drivers/note/service-account/credentials/"));

        let authenticated = store.authenticate(&credential.token).unwrap();
        assert_eq!(authenticated.subject.path, "/drivers/note/service-account");
        assert_eq!(authenticated.driver_path.as_deref(), Some("/drivers/note"));
    }

    #[test]
    fn role_path_scopes_round_trip_and_are_validated() {
        let mut store = Store::memory().unwrap();
        let role = store
            .create_role(CreateRole {
                path: "/roles/team-a/note-reader".into(),
                name: "note-reader".into(),
                description: "read team notes".into(),
                rules: vec![Rule {
                    resources: vec!["resources/note".into()],
                    verbs: vec!["get".into(), "watch".into()],
                    paths: vec!["/notes/team-a/**".into()],
                }],
            })
            .unwrap();
        assert_eq!(
            store
                .list_roles()
                .unwrap()
                .into_iter()
                .find(|candidate| candidate.path == role.path)
                .unwrap()
                .rules[0]
                .paths,
            vec!["/notes/team-a/**"]
        );

        let error = store
            .create_role(CreateRole {
                path: "/roles/team-a/invalid".into(),
                name: "invalid".into(),
                description: String::new(),
                rules: vec![Rule {
                    resources: vec!["resources/note".into()],
                    verbs: vec!["get".into()],
                    paths: vec!["notes/**".into()],
                }],
            })
            .unwrap_err();
        assert!(matches!(error, StoreError::Invalid(_)));
    }

    #[test]
    fn links_accept_identity_and_rbac_objects_as_endpoints() {
        let mut store = Store::memory().unwrap();
        let user = store
            .create_user(CreateUser {
                path: "/users/alice".into(),
                name: "alice".into(),
            })
            .unwrap();
        let service_account = store
            .create_service_account(CreateServiceAccount {
                path: "/service-accounts/automation".into(),
                name: "automation".into(),
            })
            .unwrap();
        let role = store
            .create_role(CreateRole {
                path: "/roles/automation".into(),
                name: "automation".into(),
                description: "automation role".into(),
                rules: Vec::new(),
            })
            .unwrap();
        let role_binding = store
            .create_role_binding(CreateRoleBinding {
                path: "/role-bindings/automation".into(),
                name: "automation".into(),
                role_path: role.path.clone(),
                subjects: vec![Subject {
                    kind: SubjectKind::ServiceAccount,
                    path: service_account.path.clone(),
                }],
            })
            .unwrap();
        let credential = store
            .issue_service_account_credential(&service_account.path)
            .unwrap();

        let endpoints = [
            (ObjectKind::User, user.path),
            (ObjectKind::ServiceAccount, service_account.path),
            (ObjectKind::Role, role.path.clone()),
            (ObjectKind::RoleBinding, role_binding.path),
            (ObjectKind::Credential, credential.path),
        ];
        for (index, (kind, path)) in endpoints.into_iter().enumerate() {
            let link = store
                .create_link(CreateLink {
                    path: format!("/links/security-object-{index}"),
                    source: ObjectRef { kind, path },
                    relation: "related_to".into(),
                    target: ObjectRef {
                        kind: ObjectKind::Role,
                        path: role.path.clone(),
                    },
                    metadata: json!({}),
                })
                .unwrap();
            assert_eq!(store.get_link(&link.path).unwrap(), link);
        }
    }
}
