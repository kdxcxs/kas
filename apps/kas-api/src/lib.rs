use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use kas_core::{
    CreateManifest, CreateResource, CreateRun, Driver, DriverReady, DriverWork, FinishRun,
    Manifest, Resource, Run, UpdateResourceStatus,
};
use kas_store::{Store, StoreError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Store>>,
}

pub fn app(store: Store) -> Router {
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
    };
    Router::new()
        .route("/health", get(health))
        .route("/manifests", get(list_manifests).post(create_manifest))
        .route("/manifests/{id}/driver", get(get_manifest_driver))
        .route("/resources", get(list_resources).post(create_resource))
        .route("/resources/{id}", get(get_resource))
        .route(
            "/resources/{id}/status",
            axum::routing::put(update_resource),
        )
        .route("/drivers/{id}", get(get_driver).patch(update_driver))
        .route("/drivers/{id}/claim", post(claim_work))
        .route("/runs", post(enqueue_run))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/result", axum::routing::put(finish_run))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true}))
}

async fn create_manifest(
    State(state): State<AppState>,
    Json(input): Json<CreateManifest>,
) -> ApiResult<(StatusCode, Json<Manifest>)> {
    let manifest = lock(&state)?.create_manifest(input)?;
    Ok((StatusCode::CREATED, Json(manifest)))
}

async fn list_manifests(State(state): State<AppState>) -> ApiResult<Json<Vec<Manifest>>> {
    Ok(Json(lock(&state)?.list_manifests()?))
}

async fn get_manifest_driver(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Driver>> {
    Ok(Json(lock(&state)?.driver_for_manifest(id)?))
}

async fn create_resource(
    State(state): State<AppState>,
    Json(input): Json<CreateResource>,
) -> ApiResult<(StatusCode, Json<Resource>)> {
    let resource = lock(&state)?.create_resource(input)?;
    Ok((StatusCode::CREATED, Json(resource)))
}

async fn list_resources(State(state): State<AppState>) -> ApiResult<Json<Vec<Resource>>> {
    Ok(Json(lock(&state)?.list_resources()?))
}

async fn get_resource(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Resource>> {
    Ok(Json(lock(&state)?.get_resource(id)?))
}

async fn update_resource(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<UpdateResourceStatus>,
) -> ApiResult<Json<Resource>> {
    Ok(Json(lock(&state)?.update_resource_status(id, input)?))
}

async fn get_driver(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Driver>> {
    Ok(Json(lock(&state)?.get_driver(id)?))
}

#[derive(Deserialize)]
struct ClaimWork {
    generation: u64,
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum DriverUpdate {
    Starting,
    Ready {
        generation: u64,
        process_id: u32,
        #[serde(default)]
        metadata: Value,
    },
    Stopping,
    Stopped {
        generation: u64,
    },
}

async fn update_driver(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<DriverUpdate>,
) -> ApiResult<Json<Driver>> {
    let driver = match input {
        DriverUpdate::Starting => lock(&state)?.start_driver(id)?,
        DriverUpdate::Ready {
            generation,
            process_id,
            metadata,
        } => lock(&state)?.mark_driver_ready(
            id,
            DriverReady {
                generation,
                process_id,
                metadata,
            },
        )?,
        DriverUpdate::Stopping => lock(&state)?.stop_driver(id)?,
        DriverUpdate::Stopped { generation } => {
            lock(&state)?.mark_driver_stopped(id, generation)?
        }
    };
    Ok(Json(driver))
}

async fn enqueue_run(
    State(state): State<AppState>,
    Json(input): Json<CreateRun>,
) -> ApiResult<(StatusCode, Json<Run>)> {
    let run = lock(&state)?.enqueue_run(input)?;
    Ok((StatusCode::ACCEPTED, Json(run)))
}

async fn get_run(State(state): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Run>> {
    Ok(Json(lock(&state)?.get_run(id)?))
}

async fn claim_work(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<ClaimWork>,
) -> ApiResult<Json<Option<DriverWork>>> {
    Ok(Json(lock(&state)?.claim_driver_work(id, input.generation)?))
}

async fn finish_run(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<FinishRun>,
) -> ApiResult<Json<Run>> {
    Ok(Json(lock(&state)?.finish_run(id, input)?))
}

fn lock(state: &AppState) -> Result<std::sync::MutexGuard<'_, Store>, ApiError> {
    state.store.lock().map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store lock poisoned".into(),
        )
    })
}

type ApiResult<T> = Result<T, ApiError>;

struct ApiError(StatusCode, String);

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        let status = match error {
            StoreError::NotFound(_) => StatusCode::NOT_FOUND,
            StoreError::Invalid(_) => StatusCode::BAD_REQUEST,
            StoreError::Conflict(_) => StatusCode::CONFLICT,
            StoreError::Database(_)
            | StoreError::Serialization(_)
            | StoreError::MigrationRequired { .. }
            | StoreError::UnsupportedSchema { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, error.to_string())
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
