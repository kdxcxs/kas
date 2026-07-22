use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Action {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub id: Uuid,
    pub name: String,
    pub version: u32,
    pub description: String,
    pub resource_schema: Value,
    pub actions: Vec<Action>,
    pub driver: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resource {
    pub id: Uuid,
    pub manifest_id: Uuid,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Driver {
    pub id: Uuid,
    pub manifest_id: Uuid,
    pub name: String,
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
    pub id: Uuid,
    pub request_id: Uuid,
    pub resource_id: Uuid,
    pub driver_id: Uuid,
    pub driver_generation: Option<u64>,
    pub action: String,
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
    pub id: Uuid,
    pub run_id: Uuid,
    pub sequence: u64,
    pub kind: String,
    pub data: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Manifest,
    Resource,
    Driver,
    Run,
    Event,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectRef {
    pub kind: ObjectKind,
    pub id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Link {
    pub id: Uuid,
    pub source: ObjectRef,
    pub relation: String,
    pub target: ObjectRef,
    pub metadata: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateManifest {
    pub name: String,
    pub version: u32,
    pub description: String,
    pub resource_schema: Value,
    pub actions: Vec<Action>,
    pub driver: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateResource {
    pub manifest_id: Uuid,
    pub name: String,
    pub spec: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRun {
    pub request_id: Uuid,
    pub resource_id: Uuid,
    pub action: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinishRun {
    pub driver_generation: u64,
    #[serde(flatten)]
    pub result: RunResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunResult {
    Succeeded { output: Value },
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverReady {
    pub generation: u64,
    pub process_id: u32,
    #[serde(default)]
    pub metadata: Value,
}
