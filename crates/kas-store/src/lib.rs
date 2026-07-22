use std::path::Path;

use chrono::{DateTime, Utc};
use kas_auth::{
    AuthContext, CreateRole, CreateRoleBinding, CreateServiceAccount, CreateUser, IssuedCredential,
    Role, RoleBinding, Rule, ServiceAccount, Subject, SubjectKind, User, SYSTEM_ADMIN_ROLE,
    SYSTEM_DRIVER_ROLE,
};
use kas_core::{
    Action, CreateManifest, CreateResource, CreateRun, Driver, DriverReady, DriverState,
    DriverWork, FinishRun, Manifest, Resource, Run, RunResult, RunStatus, UpdateResourceStatus,
};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
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

pub const LATEST_SCHEMA_VERSION: u32 = 3;

const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_initial.sql")),
    (2, include_str!("../migrations/0002_reconciliations.sql")),
    (3, include_str!("../migrations/0003_rbac.sql")),
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
        validate_name("Driver name", &input.driver)?;
        if input.version == 0 {
            return Err(StoreError::Invalid(
                "Manifest version must start at 1".into(),
            ));
        }
        let now = Utc::now();
        let manifest_id = Uuid::new_v4();
        let driver_id = Uuid::new_v4();
        let actions_json = serde_json::to_string(&input.actions)?;
        let schema_json = serde_json::to_string(&input.resource_schema)?;
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO manifests(id,name,version,description,resource_schema_json,actions_json,driver_name,created_at)
             VALUES (?,?,?,?,?,?,?,?)",
            params![
                manifest_id.to_string(), input.name, input.version, input.description,
                schema_json, actions_json, input.driver, stamp(now)
            ],
        )
        .map_err(|error| constraint(error, "Manifest name and version already exist"))?;
        tx.execute(
            "INSERT INTO drivers(id,manifest_id,name,state,generation,process_id,metadata_json,started_at,heartbeat_at,stopped_at,error,created_at,updated_at)
             VALUES (?,?,?,'stopped',0,NULL,'{}',NULL,NULL,?,NULL,?,?)",
            params![driver_id.to_string(), manifest_id.to_string(), input.driver, stamp(now), stamp(now), stamp(now)],
        )?;
        let service_account_id = Uuid::new_v4();
        let role_binding_id = Uuid::new_v4();
        let identity_name = format!("system:driver:{driver_id}");
        tx.execute(
            "INSERT INTO service_accounts(id,name,driver_id,managed_by,created_at) VALUES (?,?,?,'system',?)",
            params![service_account_id.to_string(), identity_name, driver_id.to_string(), stamp(now)],
        )?;
        tx.execute(
            "INSERT INTO role_bindings(id,name,role_id,managed_by,created_at) VALUES (?,?,?,'system',?)",
            params![role_binding_id.to_string(), format!("system:driver:{driver_id}"), SYSTEM_DRIVER_ROLE, stamp(now)],
        )?;
        tx.execute(
            "INSERT INTO role_binding_subjects(role_binding_id,subject_kind,subject_id) VALUES (?,'service_account',?)",
            params![role_binding_id.to_string(), service_account_id.to_string()],
        )?;
        tx.commit()?;
        Ok(Manifest {
            id: manifest_id,
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

    pub fn create_resource(&mut self, input: CreateResource) -> Result<Resource, StoreError> {
        validate_name("Resource name", &input.name)?;
        let now = Utc::now();
        let id = Uuid::new_v4();
        let spec = serde_json::to_string(&input.spec)?;
        self.connection
            .execute(
                "INSERT INTO resources(id,manifest_id,name,spec_json,status_json,revision,created_at,updated_at)
                 VALUES (?,?,?,?,'{}',0,?,?)",
                params![id.to_string(), input.manifest_id.to_string(), input.name, spec, stamp(now), stamp(now)],
            )
            .map_err(|error| constraint(error, "Manifest does not exist"))?;
        Ok(Resource {
            id,
            manifest_id: input.manifest_id,
            name: input.name,
            spec: input.spec,
            status: Value::Object(Default::default()),
            revision: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn list_resources(&self) -> Result<Vec<Resource>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,manifest_id,name,spec_json,status_json,revision,created_at,updated_at
             FROM resources ORDER BY created_at,id",
        )?;
        let rows = statement.query_map([], resource_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_resource(&self, resource_id: Uuid) -> Result<Resource, StoreError> {
        self.connection
            .query_row(
                RESOURCE_SELECT_BY_ID,
                [resource_id.to_string()],
                resource_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Resource {resource_id}")))
    }

    pub fn update_resource_status(
        &mut self,
        resource_id: Uuid,
        input: UpdateResourceStatus,
    ) -> Result<Resource, StoreError> {
        let now = Utc::now();
        let status_json = serde_json::to_string(&input.status)?;
        let changed = self.connection.execute(
            "UPDATE resources SET status_json=?,observed_revision=?,claimed_revision=NULL,
             claim_driver_generation=NULL,updated_at=?
             WHERE id=? AND revision=? AND claimed_revision=? AND claim_driver_generation=?
             AND EXISTS(
                SELECT 1 FROM drivers d
                WHERE d.id=? AND d.manifest_id=resources.manifest_id
                AND d.generation=? AND d.state='ready'
             )",
            params![
                status_json,
                input.observed_revision,
                stamp(now),
                resource_id.to_string(),
                input.observed_revision,
                input.observed_revision,
                input.driver_generation,
                input.driver_id.to_string(),
                input.driver_generation,
            ],
        )?;
        if changed != 1 {
            let resource = self.get_resource(resource_id)?;
            let observed_revision: i64 = self.connection.query_row(
                "SELECT observed_revision FROM resources WHERE id=?",
                [resource_id.to_string()],
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
        self.get_resource(resource_id)
    }

    pub fn driver_for_manifest(&self, manifest_id: Uuid) -> Result<Driver, StoreError> {
        self.connection
            .query_row(
                DRIVER_SELECT_BY_MANIFEST,
                [manifest_id.to_string()],
                driver_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Driver for Manifest {manifest_id}")))
    }

    pub fn get_driver(&self, driver_id: Uuid) -> Result<Driver, StoreError> {
        self.connection
            .query_row(
                DRIVER_SELECT_BY_ID,
                [driver_id.to_string()],
                driver_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Driver {driver_id}")))
    }

    pub fn start_driver(&mut self, driver_id: Uuid) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET state='starting',generation=generation+1,process_id=NULL,
             metadata_json='{}',started_at=?,heartbeat_at=NULL,stopped_at=NULL,error=NULL,updated_at=?
             WHERE id=? AND state IN ('stopped','failed')",
            params![stamp(now), stamp(now), driver_id.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver can only start from stopped or failed".into(),
            ));
        }
        self.get_driver(driver_id)
    }

    pub fn mark_driver_ready(
        &mut self,
        driver_id: Uuid,
        input: DriverReady,
    ) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let metadata = serde_json::to_string(&input.metadata)?;
        let changed = self.connection.execute(
            "UPDATE drivers SET state='ready',process_id=?,metadata_json=?,heartbeat_at=?,updated_at=?
             WHERE id=? AND generation=? AND state='starting'",
            params![input.process_id, metadata, stamp(now), stamp(now), driver_id.to_string(), input.generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver generation is stale or not starting".into(),
            ));
        }
        self.get_driver(driver_id)
    }

    pub fn heartbeat_driver(
        &mut self,
        driver_id: Uuid,
        generation: u64,
    ) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET heartbeat_at=?,updated_at=?
             WHERE id=? AND generation=? AND state='ready'",
            params![stamp(now), stamp(now), driver_id.to_string(), generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver generation is stale or not ready".into(),
            ));
        }
        self.get_driver(driver_id)
    }

    pub fn stop_driver(&mut self, driver_id: Uuid) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET state='stopping',updated_at=?
             WHERE id=? AND state IN ('starting','ready')",
            params![stamp(now), driver_id.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver can only stop from starting or ready".into(),
            ));
        }
        self.get_driver(driver_id)
    }

    pub fn mark_driver_stopped(
        &mut self,
        driver_id: Uuid,
        generation: u64,
    ) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET state='stopped',process_id=NULL,heartbeat_at=NULL,stopped_at=?,updated_at=?
             WHERE id=? AND generation=? AND state='stopping'",
            params![stamp(now), stamp(now), driver_id.to_string(), generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver generation is stale or not stopping".into(),
            ));
        }
        self.get_driver(driver_id)
    }

    pub fn enqueue_run(&mut self, input: CreateRun) -> Result<Run, StoreError> {
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
        let (driver_id, actions_json): (String, String) = self
            .connection
            .query_row(
                "SELECT d.id,m.actions_json FROM resources r
                 JOIN manifests m ON m.id=r.manifest_id
                 JOIN drivers d ON d.manifest_id=m.id WHERE r.id=?",
                [input.resource_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Resource {}", input.resource_id)))?;
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
        let id = Uuid::new_v4();
        self.connection.execute(
            "INSERT INTO runs(id,request_id,resource_id,driver_id,driver_generation,action,input_json,status,output_json,error,created_at,started_at,finished_at,next_event_sequence)
             VALUES (?,?,?,?,NULL,?,?,'queued',NULL,NULL,?,NULL,NULL,1)",
            params![id.to_string(), input.request_id.to_string(), input.resource_id.to_string(), driver_id, input.action, serde_json::to_string(&input.input)?, stamp(now)],
        )?;
        self.get_run(id)
    }

    pub fn claim_run(
        &mut self,
        driver_id: Uuid,
        generation: u64,
    ) -> Result<Option<Run>, StoreError> {
        let tx = self.connection.transaction()?;
        let ready: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM drivers WHERE id=? AND generation=? AND state='ready')",
            params![driver_id.to_string(), generation],
            |row| row.get(0),
        )?;
        if !ready {
            return Err(StoreError::Conflict("Driver is stale or not ready".into()));
        }
        let run_id: Option<String> = tx
            .query_row(
                "SELECT id FROM runs WHERE driver_id=? AND status='queued' ORDER BY created_at,id LIMIT 1",
                [driver_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(run_id) = run_id else {
            tx.commit()?;
            return Ok(None);
        };
        let now = Utc::now();
        tx.execute(
            "UPDATE runs SET status='running',driver_generation=?,started_at=? WHERE id=? AND status='queued'",
            params![generation, stamp(now), run_id],
        )?;
        append_event(
            &tx,
            &run_id,
            "run.started",
            &Value::Object(Default::default()),
            now,
        )?;
        tx.commit()?;
        Ok(Some(self.get_run(parse_uuid(&run_id, 0)?)?))
    }

    pub fn claim_driver_work(
        &mut self,
        driver_id: Uuid,
        generation: u64,
    ) -> Result<Option<DriverWork>, StoreError> {
        if let Some(resource) = self.claim_reconciliation(driver_id, generation)? {
            return Ok(Some(DriverWork::Reconcile {
                revision: resource.revision,
                resource,
            }));
        }
        let Some(run) = self.claim_run(driver_id, generation)? else {
            return Ok(None);
        };
        let resource = self.get_resource(run.resource_id)?;
        Ok(Some(DriverWork::Run {
            run: Box::new(run),
            resource,
        }))
    }

    fn claim_reconciliation(
        &mut self,
        driver_id: Uuid,
        generation: u64,
    ) -> Result<Option<Resource>, StoreError> {
        let tx = self.connection.transaction()?;
        let manifest_id: Option<String> = tx
            .query_row(
                "SELECT manifest_id FROM drivers WHERE id=? AND generation=? AND state='ready'",
                params![driver_id.to_string(), generation],
                |row| row.get(0),
            )
            .optional()?;
        let Some(manifest_id) = manifest_id else {
            return Err(StoreError::Conflict("Driver is stale or not ready".into()));
        };
        let resource: Option<(String, u64)> = tx
            .query_row(
                 "SELECT id,revision FROM resources
                 WHERE manifest_id=? AND observed_revision < revision
                 AND (claimed_revision IS NULL OR claimed_revision != revision OR claim_driver_generation != ?)
                 ORDER BY created_at,id LIMIT 1",
                params![manifest_id, generation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((resource_id, revision)) = resource else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "UPDATE resources SET claimed_revision=?,claim_driver_generation=? WHERE id=?",
            params![revision, generation, resource_id],
        )?;
        tx.commit()?;
        Ok(Some(self.get_resource(parse_uuid(&resource_id, 0)?)?))
    }

    pub fn finish_run(&mut self, run_id: Uuid, input: FinishRun) -> Result<Run, StoreError> {
        let existing = self.get_run(run_id)?;
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
                return Ok(existing);
            }
            return Err(StoreError::Conflict(
                "Run already has a different result".into(),
            ));
        }
        let tx = self.connection.transaction()?;
        let now = Utc::now();
        let (status, output, error, event_kind, event_data) = match input.result {
            RunResult::Succeeded { output } => (
                "succeeded",
                Some(serde_json::to_string(&output)?),
                None,
                "run.succeeded",
                output,
            ),
            RunResult::Failed { error } => (
                "failed",
                None,
                Some(error.clone()),
                "run.failed",
                serde_json::json!({ "error": error }),
            ),
        };
        let changed = tx.execute(
            "UPDATE runs SET status=?,output_json=?,error=?,finished_at=?
             WHERE id=? AND status='running' AND driver_generation=?
             AND EXISTS(SELECT 1 FROM drivers d WHERE d.id=runs.driver_id AND d.generation=? AND d.state='ready')",
            params![status, output, error, stamp(now), run_id.to_string(), input.driver_generation, input.driver_generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Run is not owned by the current Driver generation".into(),
            ));
        }
        append_event(&tx, &run_id.to_string(), event_kind, &event_data, now)?;
        tx.commit()?;
        self.get_run(run_id)
    }

    pub fn get_run(&self, run_id: Uuid) -> Result<Run, StoreError> {
        self.connection
            .query_row(RUN_SELECT_BY_ID, [run_id.to_string()], run_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Run {run_id}")))
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
        let user_id = Uuid::new_v4();
        let binding_id = Uuid::new_v4();
        let token = kas_auth::issue_token();
        let credential_id = Uuid::new_v4();
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO users(id,name,disabled,created_at) VALUES (?,?,0,?)",
            params![user_id.to_string(), name, stamp(now)],
        )?;
        tx.execute(
            "INSERT INTO role_bindings(id,name,role_id,managed_by,created_at) VALUES (?,?,?,'system',?)",
            params![binding_id.to_string(), "system:bootstrap-admin", SYSTEM_ADMIN_ROLE, stamp(now)],
        )?;
        tx.execute(
            "INSERT INTO role_binding_subjects(role_binding_id,subject_kind,subject_id) VALUES (?,'user',?)",
            params![binding_id.to_string(), user_id.to_string()],
        )?;
        tx.execute(
            "INSERT INTO credentials(id,subject_kind,subject_id,token_hash,driver_generation,expires_at,revoked_at,created_at)
             VALUES (?,'user',?,?,NULL,NULL,NULL,?)",
            params![credential_id.to_string(), user_id.to_string(), kas_auth::token_hash(&token), stamp(now)],
        )?;
        tx.commit()?;
        Ok(IssuedCredential {
            id: credential_id,
            token,
            expires_at: None,
        })
    }

    pub fn authenticate(&self, token: &str) -> Result<AuthContext, StoreError> {
        let hash = kas_auth::token_hash(token);
        let now = stamp(Utc::now());
        let row: Option<(String, String, Option<u64>, Option<String>)> = self.connection
            .query_row(
                "SELECT c.subject_kind,c.subject_id,c.driver_generation,sa.driver_id
                 FROM credentials c
                 LEFT JOIN users u ON c.subject_kind='user' AND u.id=c.subject_id
                 LEFT JOIN service_accounts sa ON c.subject_kind='service_account' AND sa.id=c.subject_id
                 WHERE c.token_hash=? AND c.revoked_at IS NULL
                 AND (c.expires_at IS NULL OR c.expires_at>?)
                 AND ((c.subject_kind='user' AND u.disabled=0) OR (c.subject_kind='service_account' AND sa.id IS NOT NULL))",
                params![hash, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((kind, subject_id, driver_generation, driver_id)) = row else {
            return Err(StoreError::Invalid("Invalid or expired credential".into()));
        };
        let kind = parse_subject_kind(&kind)?;
        let subject_id = parse_uuid(&subject_id, 0)?;
        let driver_id = driver_id.map(|value| parse_uuid(&value, 0)).transpose()?;
        if let (Some(driver_id), Some(generation)) = (driver_id, driver_generation) {
            let driver = self.get_driver(driver_id)?;
            if driver.generation != generation {
                return Err(StoreError::Invalid(
                    "Driver credential generation is stale".into(),
                ));
            }
        }
        let mut statement = self.connection.prepare(
            "SELECT r.rules_json FROM roles r
             JOIN role_bindings rb ON rb.role_id=r.id
             JOIN role_binding_subjects rbs ON rbs.role_binding_id=rb.id
             WHERE rbs.subject_kind=? AND rbs.subject_id=?",
        )?;
        let rows = statement.query_map(params![kind.as_str(), subject_id.to_string()], |row| {
            row.get::<_, String>(0)
        })?;
        let mut rules = Vec::new();
        for row in rows {
            rules.extend(serde_json::from_str::<Vec<Rule>>(&row?)?);
        }
        Ok(AuthContext {
            subject: Subject {
                kind,
                id: subject_id,
            },
            rules,
            driver_id,
            driver_generation,
        })
    }

    pub fn issue_driver_credential(
        &mut self,
        driver_id: Uuid,
    ) -> Result<IssuedCredential, StoreError> {
        let driver = self.get_driver(driver_id)?;
        if driver.state != DriverState::Starting {
            return Err(StoreError::Conflict(
                "Driver must be starting before credentials are issued".into(),
            ));
        }
        let service_account_id: String = self.connection.query_row(
            "SELECT id FROM service_accounts WHERE driver_id=? AND managed_by='system'",
            [driver_id.to_string()],
            |row| row.get(0),
        )?;
        self.issue_credential(
            Subject {
                kind: SubjectKind::ServiceAccount,
                id: parse_uuid(&service_account_id, 0)?,
            },
            Some(driver.generation),
            Some(Utc::now() + chrono::Duration::hours(1)),
            true,
        )
    }

    pub fn issue_user_credential(&mut self, user_id: Uuid) -> Result<IssuedCredential, StoreError> {
        self.get_user(user_id)?;
        self.issue_credential(
            Subject {
                kind: SubjectKind::User,
                id: user_id,
            },
            None,
            None,
            false,
        )
    }

    pub fn issue_service_account_credential(
        &mut self,
        service_account_id: Uuid,
    ) -> Result<IssuedCredential, StoreError> {
        let driver_id: Option<String> = self
            .connection
            .query_row(
                "SELECT driver_id FROM service_accounts WHERE id=?",
                [service_account_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("ServiceAccount {service_account_id}")))?;
        if driver_id.is_some() {
            return Err(StoreError::Conflict(
                "Driver credentials must be issued through the Driver endpoint".into(),
            ));
        }
        self.issue_credential(
            Subject {
                kind: SubjectKind::ServiceAccount,
                id: service_account_id,
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
        let id = Uuid::new_v4();
        let token = kas_auth::issue_token();
        let tx = self.connection.transaction()?;
        if revoke_existing {
            tx.execute(
                "UPDATE credentials SET revoked_at=? WHERE subject_kind=? AND subject_id=? AND revoked_at IS NULL",
                params![stamp(now), subject.kind.as_str(), subject.id.to_string()],
            )?;
        }
        tx.execute(
            "INSERT INTO credentials(id,subject_kind,subject_id,token_hash,driver_generation,expires_at,revoked_at,created_at)
             VALUES (?,?,?,?,?,?,NULL,?)",
            params![id.to_string(), subject.kind.as_str(), subject.id.to_string(), kas_auth::token_hash(&token), driver_generation, expires_at.map(stamp), stamp(now)],
        )?;
        tx.commit()?;
        Ok(IssuedCredential {
            id,
            token,
            expires_at,
        })
    }

    pub fn create_user(&mut self, input: CreateUser) -> Result<User, StoreError> {
        validate_name("User name", &input.name)?;
        let id = Uuid::new_v4();
        let now = Utc::now();
        self.connection
            .execute(
                "INSERT INTO users(id,name,disabled,created_at) VALUES (?,?,0,?)",
                params![id.to_string(), input.name, stamp(now)],
            )
            .map_err(|error| constraint(error, "User name already exists"))?;
        Ok(User {
            id,
            name: input.name,
            disabled: false,
            created_at: now,
        })
    }

    pub fn get_user(&self, id: Uuid) -> Result<User, StoreError> {
        self.connection
            .query_row(
                "SELECT id,name,disabled,created_at FROM users WHERE id=?",
                [id.to_string()],
                user_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("User {id}")))
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
        let id = Uuid::new_v4();
        let now = Utc::now();
        self.connection.execute(
            "INSERT INTO service_accounts(id,name,driver_id,managed_by,created_at) VALUES (?,?,NULL,'user',?)",
            params![id.to_string(), input.name, stamp(now)],
        ).map_err(|error| constraint(error, "ServiceAccount name already exists"))?;
        Ok(ServiceAccount {
            id,
            name: input.name,
            driver_id: None,
            managed_by: "user".into(),
            created_at: now,
        })
    }

    pub fn create_role(&mut self, input: CreateRole) -> Result<Role, StoreError> {
        validate_name("Role name", &input.name)?;
        let id = Uuid::new_v4();
        let now = Utc::now();
        self.connection.execute(
            "INSERT INTO roles(id,name,description,rules_json,managed_by,created_at,updated_at) VALUES (?,?,?,?,'user',?,?)",
            params![id.to_string(), input.name, input.description, serde_json::to_string(&input.rules)?, stamp(now), stamp(now)],
        ).map_err(|error| constraint(error, "Role name already exists"))?;
        Ok(Role {
            id,
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

    pub fn update_role(&mut self, id: Uuid, input: CreateRole) -> Result<Role, StoreError> {
        validate_name("Role name", &input.name)?;
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE roles SET name=?,description=?,rules_json=?,updated_at=? WHERE id=? AND managed_by='user'",
            params![input.name, input.description, serde_json::to_string(&input.rules)?, stamp(now), id.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "System Role cannot be modified".into(),
            ));
        }
        self.connection.query_row(
            "SELECT id,name,description,rules_json,managed_by,created_at,updated_at FROM roles WHERE id=?",
            [id.to_string()], role_from_row,
        ).map_err(StoreError::from)
    }

    pub fn delete_role(&mut self, id: Uuid) -> Result<(), StoreError> {
        let changed = self
            .connection
            .execute(
                "DELETE FROM roles WHERE id=? AND managed_by='user'",
                [id.to_string()],
            )
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
        if input.subjects.is_empty() {
            return Err(StoreError::Invalid("RoleBinding requires a subject".into()));
        }
        let id = Uuid::new_v4();
        let now = Utc::now();
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO role_bindings(id,name,role_id,managed_by,created_at) VALUES (?,?,?,'user',?)",
            params![id.to_string(), input.name, input.role_id.to_string(), stamp(now)],
        ).map_err(|error| constraint(error, "Role or RoleBinding is invalid"))?;
        for subject in &input.subjects {
            ensure_subject_exists(&tx, subject)?;
            tx.execute(
                "INSERT INTO role_binding_subjects(role_binding_id,subject_kind,subject_id) VALUES (?,?,?)",
                params![id.to_string(), subject.kind.as_str(), subject.id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(RoleBinding {
            id,
            name: input.name,
            role_id: input.role_id,
            subjects: input.subjects,
            managed_by: "user".into(),
            created_at: now,
        })
    }

    pub fn list_role_bindings(&self) -> Result<Vec<RoleBinding>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,name,role_id,managed_by,created_at FROM role_bindings ORDER BY name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                uuid_from_row(row, 0)?,
                row.get::<_, String>(1)?,
                uuid_from_row(row, 2)?,
                row.get::<_, String>(3)?,
                time_from_row(row, 4)?,
            ))
        })?;
        let base = rows.collect::<Result<Vec<_>, _>>()?;
        let mut bindings = Vec::new();
        for (id, name, role_id, managed_by, created_at) in base {
            let mut subjects_statement = self.connection.prepare(
                "SELECT subject_kind,subject_id FROM role_binding_subjects WHERE role_binding_id=? ORDER BY subject_kind,subject_id",
            )?;
            let subjects = subjects_statement
                .query_map([id.to_string()], |row| {
                    let kind: String = row.get(0)?;
                    let kind = match kind.as_str() {
                        "user" => SubjectKind::User,
                        "service_account" => SubjectKind::ServiceAccount,
                        other => return Err(from_sql(0, format!("invalid subject kind {other}"))),
                    };
                    Ok(Subject {
                        kind,
                        id: uuid_from_row(row, 1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            bindings.push(RoleBinding {
                id,
                name,
                role_id,
                subjects,
                managed_by,
                created_at,
            });
        }
        Ok(bindings)
    }

    pub fn delete_role_binding(&mut self, id: Uuid) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "DELETE FROM role_bindings WHERE id=? AND managed_by='user'",
            [id.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "System RoleBinding cannot be deleted".into(),
            ));
        }
        Ok(())
    }
}

fn append_event(
    tx: &Transaction<'_>,
    run_id: &str,
    kind: &str,
    data: &Value,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let sequence: u64 = tx.query_row(
        "UPDATE runs SET next_event_sequence=next_event_sequence+1 WHERE id=? RETURNING next_event_sequence-1",
        [run_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO events(id,run_id,sequence,kind,data_json,created_at) VALUES (?,?,?,?,?,?)",
        params![
            Uuid::new_v4().to_string(),
            run_id,
            sequence,
            kind,
            serde_json::to_string(data)?,
            stamp(now)
        ],
    )?;
    Ok(())
}

const DRIVER_SELECT_BY_ID: &str = "SELECT id,manifest_id,name,state,generation,process_id,metadata_json,started_at,heartbeat_at,stopped_at,error,created_at,updated_at FROM drivers WHERE id=?";
const DRIVER_SELECT_BY_MANIFEST: &str = "SELECT id,manifest_id,name,state,generation,process_id,metadata_json,started_at,heartbeat_at,stopped_at,error,created_at,updated_at FROM drivers WHERE manifest_id=?";
const RESOURCE_SELECT_BY_ID: &str = "SELECT id,manifest_id,name,spec_json,status_json,revision,created_at,updated_at FROM resources WHERE id=?";
const RUN_SELECT_BY_ID: &str = "SELECT id,request_id,resource_id,driver_id,driver_generation,action,input_json,status,output_json,error,created_at,started_at,finished_at FROM runs WHERE id=?";
const RUN_SELECT_BY_REQUEST: &str = "SELECT id,request_id,resource_id,driver_id,driver_generation,action,input_json,status,output_json,error,created_at,started_at,finished_at FROM runs WHERE request_id=?";

fn manifest_from_row(row: &Row<'_>) -> rusqlite::Result<Manifest> {
    Ok(Manifest {
        id: uuid_from_row(row, 0)?,
        name: row.get(1)?,
        version: row.get(2)?,
        description: row.get(3)?,
        resource_schema: json_from_row(row, 4)?,
        actions: json_from_row(row, 5)?,
        driver: row.get(6)?,
        created_at: time_from_row(row, 7)?,
    })
}

fn resource_from_row(row: &Row<'_>) -> rusqlite::Result<Resource> {
    Ok(Resource {
        id: uuid_from_row(row, 0)?,
        manifest_id: uuid_from_row(row, 1)?,
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
        id: uuid_from_row(row, 0)?,
        name: row.get(1)?,
        disabled: row.get(2)?,
        created_at: time_from_row(row, 3)?,
    })
}

fn role_from_row(row: &Row<'_>) -> rusqlite::Result<Role> {
    Ok(Role {
        id: uuid_from_row(row, 0)?,
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
    let table = match subject.kind {
        SubjectKind::User => "users",
        SubjectKind::ServiceAccount => "service_accounts",
    };
    let exists: bool = tx.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?)"),
        [subject.id.to_string()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StoreError::NotFound(format!(
            "{} {}",
            subject.kind.as_str(),
            subject.id
        )));
    }
    Ok(())
}

fn driver_from_row(row: &Row<'_>) -> rusqlite::Result<Driver> {
    Ok(Driver {
        id: uuid_from_row(row, 0)?,
        manifest_id: uuid_from_row(row, 1)?,
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
        id: uuid_from_row(row, 0)?,
        request_id: uuid_from_row(row, 1)?,
        resource_id: uuid_from_row(row, 2)?,
        driver_id: uuid_from_row(row, 3)?,
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

fn validate_name(label: &str, value: &str) -> Result<(), StoreError> {
    if value.trim().is_empty() || value.chars().count() > 128 {
        return Err(StoreError::Invalid(format!("{label} is empty or too long")));
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

    fn manifest() -> CreateManifest {
        CreateManifest {
            name: "example".into(),
            version: 1,
            description: "Example".into(),
            resource_schema: json!({"type":"object"}),
            actions: vec![Action {
                name: "execute".into(),
                description: "Execute".into(),
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
            }],
            driver: "example-driver".into(),
        }
    }

    #[test]
    fn manifest_owns_exactly_one_stable_driver() {
        let mut store = Store::memory().unwrap();
        let created_manifest = store.create_manifest(manifest()).unwrap();
        let driver = store.driver_for_manifest(created_manifest.id).unwrap();
        assert_eq!(driver.state, DriverState::Stopped);
        assert_eq!(driver.generation, 0);
        assert!(store.create_manifest(manifest()).is_err());
    }

    #[test]
    fn migration_is_explicit_and_store_refuses_an_unmigrated_database() {
        let path = std::env::temp_dir().join(format!("kas-store-{}.db", Uuid::new_v4()));
        let error = Store::open(&path).err().unwrap();
        assert!(matches!(
            error,
            StoreError::MigrationRequired {
                current: 0,
                latest: 3
            }
        ));
        assert_eq!(migrate(&path).unwrap(), 3);
        Store::open(&path).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn migration_upgrades_a_version_one_database() {
        let path = std::env::temp_dir().join(format!("kas-store-v1-{}.db", Uuid::new_v4()));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(include_str!("../migrations/0001_initial.sql"))
            .unwrap();
        connection
            .pragma_update(None, "user_version", 1_u32)
            .unwrap();
        drop(connection);

        assert_eq!(migrate(&path).unwrap(), 3);
        Store::open(&path).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn resources_share_the_manifest_singleton_driver() {
        let mut store = Store::memory().unwrap();
        let manifest = store.create_manifest(manifest()).unwrap();
        let driver = store.driver_for_manifest(manifest.id).unwrap();
        for name in ["one", "two"] {
            let resource = store
                .create_resource(CreateResource {
                    manifest_id: manifest.id,
                    name: name.into(),
                    spec: json!({}),
                })
                .unwrap();
            let run = store
                .enqueue_run(CreateRun {
                    request_id: Uuid::new_v4(),
                    resource_id: resource.id,
                    action: "execute".into(),
                    input: json!({}),
                })
                .unwrap();
            assert_eq!(run.driver_id, driver.id);
        }
    }

    #[test]
    fn driver_claim_is_scoped_and_generation_fenced() {
        let mut store = Store::memory().unwrap();
        let first_manifest = store.create_manifest(manifest()).unwrap();
        let mut second_declaration = manifest();
        second_declaration.name = "other".into();
        let second_manifest = store.create_manifest(second_declaration).unwrap();
        let first_resource = store
            .create_resource(CreateResource {
                manifest_id: first_manifest.id,
                name: "first".into(),
                spec: json!({}),
            })
            .unwrap();
        store
            .create_resource(CreateResource {
                manifest_id: second_manifest.id,
                name: "second".into(),
                spec: json!({}),
            })
            .unwrap();
        let driver = store.driver_for_manifest(first_manifest.id).unwrap();
        let starting = store.start_driver(driver.id).unwrap();
        let ready = store
            .mark_driver_ready(
                driver.id,
                DriverReady {
                    generation: starting.generation,
                    process_id: 123,
                    metadata: json!({}),
                },
            )
            .unwrap();

        let work = store
            .claim_driver_work(driver.id, ready.generation)
            .unwrap()
            .unwrap();
        let claimed_resource = match work {
            DriverWork::Reconcile {
                resource,
                revision: 0,
            } => resource,
            other => panic!("unexpected work: {other:?}"),
        };
        assert_eq!(claimed_resource.id, first_resource.id);
        let status = UpdateResourceStatus {
            driver_id: driver.id,
            driver_generation: ready.generation,
            observed_revision: claimed_resource.revision,
            status: json!({ "ready": true }),
        };
        let first_update = store
            .update_resource_status(claimed_resource.id, status.clone())
            .unwrap();
        let repeated_update = store
            .update_resource_status(claimed_resource.id, status)
            .unwrap();
        assert_eq!(first_update, repeated_update);
        assert!(store
            .claim_driver_work(driver.id, ready.generation)
            .unwrap()
            .is_none());
        assert!(store
            .claim_driver_work(driver.id, ready.generation + 1)
            .is_err());
    }

    #[test]
    fn enqueue_run_validates_input_against_the_action_schema() {
        let mut store = Store::memory().unwrap();
        let mut declaration = manifest();
        declaration.actions[0].input_schema = json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" }
            },
            "required": ["command"],
            "additionalProperties": false
        });
        let manifest = store.create_manifest(declaration).unwrap();
        let resource = store
            .create_resource(CreateResource {
                manifest_id: manifest.id,
                name: "one".into(),
                spec: json!({}),
            })
            .unwrap();
        let request_id = Uuid::new_v4();

        let error = store
            .enqueue_run(CreateRun {
                request_id,
                resource_id: resource.id,
                action: "execute".into(),
                input: json!({ "command": 42 }),
            })
            .unwrap_err();
        assert!(matches!(error, StoreError::Invalid(_)));

        let run = store
            .enqueue_run(CreateRun {
                request_id,
                resource_id: resource.id,
                action: "execute".into(),
                input: json!({ "command": "echo ok" }),
            })
            .unwrap();
        assert_eq!(run.status, RunStatus::Queued);
    }

    #[test]
    fn stale_driver_generation_cannot_complete_a_run() {
        let mut store = Store::memory().unwrap();
        let manifest = store.create_manifest(manifest()).unwrap();
        let resource = store
            .create_resource(CreateResource {
                manifest_id: manifest.id,
                name: "one".into(),
                spec: json!({}),
            })
            .unwrap();
        let driver = store.driver_for_manifest(manifest.id).unwrap();
        let starting = store.start_driver(driver.id).unwrap();
        let ready = store
            .mark_driver_ready(
                driver.id,
                DriverReady {
                    generation: starting.generation,
                    process_id: 123,
                    metadata: json!({}),
                },
            )
            .unwrap();
        let queued = store
            .enqueue_run(CreateRun {
                request_id: Uuid::new_v4(),
                resource_id: resource.id,
                action: "execute".into(),
                input: json!({}),
            })
            .unwrap();
        let running = store
            .claim_run(driver.id, ready.generation)
            .unwrap()
            .unwrap();
        assert_eq!(running.id, queued.id);
        assert!(store
            .finish_run(
                running.id,
                FinishRun {
                    driver_generation: ready.generation + 1,
                    result: RunResult::Succeeded {
                        output: json!({"wrong":true}),
                    },
                },
            )
            .is_err());
        assert_eq!(
            store.get_run(running.id).unwrap().status,
            RunStatus::Running
        );
        let result = FinishRun {
            driver_generation: ready.generation,
            result: RunResult::Succeeded {
                output: json!({"ok":true}),
            },
        };
        let completed = store.finish_run(running.id, result.clone()).unwrap();
        assert_eq!(completed.status, RunStatus::Succeeded);
        assert_eq!(store.finish_run(running.id, result).unwrap(), completed);
    }

    #[test]
    fn driver_can_report_a_failed_run_result() {
        let mut store = Store::memory().unwrap();
        let manifest = store.create_manifest(manifest()).unwrap();
        let resource = store
            .create_resource(CreateResource {
                manifest_id: manifest.id,
                name: "one".into(),
                spec: json!({}),
            })
            .unwrap();
        let driver = store.driver_for_manifest(manifest.id).unwrap();
        let starting = store.start_driver(driver.id).unwrap();
        let ready = store
            .mark_driver_ready(
                driver.id,
                DriverReady {
                    generation: starting.generation,
                    process_id: 123,
                    metadata: json!({}),
                },
            )
            .unwrap();
        store
            .enqueue_run(CreateRun {
                request_id: Uuid::new_v4(),
                resource_id: resource.id,
                action: "execute".into(),
                input: json!({}),
            })
            .unwrap();
        let running = store
            .claim_run(driver.id, ready.generation)
            .unwrap()
            .unwrap();

        let failed = store
            .finish_run(
                running.id,
                FinishRun {
                    driver_generation: ready.generation,
                    result: RunResult::Failed {
                        error: "driver rejected input".into(),
                    },
                },
            )
            .unwrap();

        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("driver rejected input"));
        assert_eq!(failed.output, None);
    }
}
