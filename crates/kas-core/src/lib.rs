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
    pub driver: Option<String>,
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
    pub sequence: u64,
    pub event_type: EventType,
    pub object_kind: ObjectKind,
    pub object_id: Uuid,
    pub manifest_id: Option<Uuid>,
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
    Resource,
    Driver,
    Run,
    Link,
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
    pub driver: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateResource {
    pub manifest_id: Uuid,
    pub name: String,
    pub spec: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateResource {
    pub expected_revision: u64,
    pub spec: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateLink {
    pub source: ObjectRef,
    pub relation: String,
    pub target: ObjectRef,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkFilter {
    pub source: Option<ObjectRef>,
    pub relation: Option<String>,
    pub target: Option<ObjectRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedResource {
    pub id: Uuid,
    pub manifest_id: Uuid,
    pub name: String,
    pub spec: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedLink {
    pub id: Uuid,
    pub source: ObjectRef,
    pub relation: String,
    pub target: ObjectRef,
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
        resource_id: Uuid,
        expected_revision: u64,
        spec: Value,
    },
    CreateLink {
        link: PlannedLink,
    },
    UpdateResourceStatus {
        resource_id: Uuid,
        observed_revision: u64,
        status: Value,
    },
    CompleteRun {
        run_id: Uuid,
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
    pub object_id: Option<Uuid>,
    pub manifest_id: Option<Uuid>,
    pub after_sequence: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateResourceStatus {
    pub driver_id: Uuid,
    pub driver_generation: u64,
    pub observed_revision: u64,
    pub status: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRun {
    pub request_id: Uuid,
    pub resource_id: Uuid,
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
    Reconcile { resource: Resource, revision: u64 },
    Run { run: Box<Run>, resource: Resource },
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
    pub driver_id: Uuid,
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
