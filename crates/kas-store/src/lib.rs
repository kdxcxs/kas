use std::path::Path;

use chrono::{DateTime, Utc};
use kas_core::{
    Action, CreateManifest, CreateResource, CreateRun, Driver, DriverReady, DriverState, FinishRun,
    Manifest, Resource, Run, RunResult, RunStatus,
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

pub const LATEST_SCHEMA_VERSION: u32 = 1;

const MIGRATIONS: &[(u32, &str)] = &[(1, include_str!("../migrations/0001_initial.sql"))];

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

    pub fn finish_run(&mut self, run_id: Uuid, input: FinishRun) -> Result<Run, StoreError> {
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
                latest: 1
            }
        ));
        assert_eq!(migrate(&path).unwrap(), 1);
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
        assert_eq!(
            store
                .finish_run(
                    running.id,
                    FinishRun {
                        driver_generation: ready.generation,
                        result: RunResult::Succeeded {
                            output: json!({"ok":true}),
                        },
                    },
                )
                .unwrap()
                .status,
            RunStatus::Succeeded
        );
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
