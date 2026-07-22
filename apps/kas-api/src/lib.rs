use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use kas_auth::{
    AuthContext, CreateRole, CreateRoleBinding, CreateServiceAccount, CreateUser, IssuedCredential,
    Role, RoleBinding, ServiceAccount, User,
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
    let protected = Router::new()
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
        .route("/drivers/{id}/credentials", post(issue_driver_credential))
        .route("/runs", post(enqueue_run))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/result", axum::routing::put(finish_run))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}/credentials", post(issue_user_credential))
        .route("/service-accounts", post(create_service_account))
        .route(
            "/service-accounts/{id}/credentials",
            post(issue_service_account_credential),
        )
        .route("/roles", get(list_roles).post(create_role))
        .route(
            "/roles/{id}",
            axum::routing::patch(update_role).delete(delete_role),
        )
        .route(
            "/role-bindings",
            get(list_role_bindings).post(create_role_binding),
        )
        .route(
            "/role-bindings/{id}",
            axum::routing::delete(delete_role_binding),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate_request,
        ));
    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true}))
}

async fn authenticate_request(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(token) = token else {
        return unauthorized().into_response();
    };
    let authenticated = state
        .store
        .lock()
        .ok()
        .and_then(|store| store.authenticate(token).ok())
        .is_some();
    if !authenticated {
        return unauthorized().into_response();
    }
    next.run(request).await
}

async fn create_manifest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateManifest>,
) -> ApiResult<(StatusCode, Json<Manifest>)> {
    require(&state, &headers, "manifests", "create")?;
    let manifest = lock(&state)?.create_manifest(input)?;
    Ok((StatusCode::CREATED, Json(manifest)))
}

async fn list_manifests(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Manifest>>> {
    require(&state, &headers, "manifests", "list")?;
    Ok(Json(lock(&state)?.list_manifests()?))
}

async fn get_manifest_driver(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Driver>> {
    require(&state, &headers, "drivers", "get")?;
    Ok(Json(lock(&state)?.driver_for_manifest(id)?))
}

async fn create_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateResource>,
) -> ApiResult<(StatusCode, Json<Resource>)> {
    require(&state, &headers, "resources", "create")?;
    let resource = lock(&state)?.create_resource(input)?;
    Ok((StatusCode::CREATED, Json(resource)))
}

async fn list_resources(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Resource>>> {
    require(&state, &headers, "resources", "list")?;
    Ok(Json(lock(&state)?.list_resources()?))
}

async fn get_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Resource>> {
    require(&state, &headers, "resources", "get")?;
    Ok(Json(lock(&state)?.get_resource(id)?))
}

async fn update_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<UpdateResourceStatus>,
) -> ApiResult<Json<Resource>> {
    let auth = require(&state, &headers, "resources/status", "update")?;
    require_driver_ownership(&auth, input.driver_id, input.driver_generation)?;
    Ok(Json(lock(&state)?.update_resource_status(id, input)?))
}

async fn get_driver(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Driver>> {
    let auth = require(&state, &headers, "drivers", "get")?;
    if let Some(driver_id) = auth.driver_id {
        if driver_id != id {
            return Err(forbidden());
        }
    }
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
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<DriverUpdate>,
) -> ApiResult<Json<Driver>> {
    let auth = require(&state, &headers, "drivers", "patch")?;
    if let Some(driver_id) = auth.driver_id {
        if driver_id != id {
            return Err(forbidden());
        }
        let generation = match &input {
            DriverUpdate::Ready { generation, .. } | DriverUpdate::Stopped { generation } => {
                *generation
            }
            DriverUpdate::Starting | DriverUpdate::Stopping => return Err(forbidden()),
        };
        require_driver_ownership(&auth, id, generation)?;
    }
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
    headers: HeaderMap,
    Json(input): Json<CreateRun>,
) -> ApiResult<(StatusCode, Json<Run>)> {
    require(&state, &headers, "runs", "create")?;
    let run = lock(&state)?.enqueue_run(input)?;
    Ok((StatusCode::ACCEPTED, Json(run)))
}

async fn get_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Run>> {
    require(&state, &headers, "runs", "get")?;
    Ok(Json(lock(&state)?.get_run(id)?))
}

async fn claim_work(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<ClaimWork>,
) -> ApiResult<Json<Option<DriverWork>>> {
    let auth = require(&state, &headers, "drivers/claim", "create")?;
    require_driver_ownership(&auth, id, input.generation)?;
    Ok(Json(lock(&state)?.claim_driver_work(id, input.generation)?))
}

async fn finish_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<FinishRun>,
) -> ApiResult<Json<Run>> {
    let auth = require(&state, &headers, "runs/result", "update")?;
    if let Some(driver_id) = auth.driver_id {
        require_driver_ownership(&auth, driver_id, input.driver_generation)?;
    }
    Ok(Json(lock(&state)?.finish_run(id, input)?))
}

async fn issue_driver_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<(StatusCode, Json<IssuedCredential>)> {
    require(&state, &headers, "drivers/credentials", "create")?;
    let credential = lock(&state)?.issue_driver_credential(id)?;
    Ok((StatusCode::CREATED, Json(credential)))
}

async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateUser>,
) -> ApiResult<(StatusCode, Json<User>)> {
    require(&state, &headers, "users", "create")?;
    Ok((StatusCode::CREATED, Json(lock(&state)?.create_user(input)?)))
}

async fn list_users(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<User>>> {
    require(&state, &headers, "users", "list")?;
    Ok(Json(lock(&state)?.list_users()?))
}

async fn issue_user_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<(StatusCode, Json<IssuedCredential>)> {
    require(&state, &headers, "credentials", "create")?;
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.issue_user_credential(id)?),
    ))
}

async fn create_service_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateServiceAccount>,
) -> ApiResult<(StatusCode, Json<ServiceAccount>)> {
    require(&state, &headers, "serviceaccounts", "create")?;
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.create_service_account(input)?),
    ))
}

async fn issue_service_account_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<(StatusCode, Json<IssuedCredential>)> {
    require(&state, &headers, "credentials", "create")?;
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.issue_service_account_credential(id)?),
    ))
}

async fn create_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateRole>,
) -> ApiResult<(StatusCode, Json<Role>)> {
    require(&state, &headers, "roles", "create")?;
    require(&state, &headers, "roles", "escalate")?;
    Ok((StatusCode::CREATED, Json(lock(&state)?.create_role(input)?)))
}

async fn list_roles(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Role>>> {
    require(&state, &headers, "roles", "list")?;
    Ok(Json(lock(&state)?.list_roles()?))
}

async fn update_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<CreateRole>,
) -> ApiResult<Json<Role>> {
    require(&state, &headers, "roles", "patch")?;
    require(&state, &headers, "roles", "escalate")?;
    Ok(Json(lock(&state)?.update_role(id, input)?))
}

async fn delete_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require(&state, &headers, "roles", "delete")?;
    lock(&state)?.delete_role(id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_role_binding(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateRoleBinding>,
) -> ApiResult<(StatusCode, Json<RoleBinding>)> {
    require(&state, &headers, "rolebindings", "create")?;
    require(&state, &headers, "roles", "bind")?;
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.create_role_binding(input)?),
    ))
}

async fn list_role_bindings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<RoleBinding>>> {
    require(&state, &headers, "rolebindings", "list")?;
    Ok(Json(lock(&state)?.list_role_bindings()?))
}

async fn delete_role_binding(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require(&state, &headers, "rolebindings", "delete")?;
    lock(&state)?.delete_role_binding(id)?;
    Ok(StatusCode::NO_CONTENT)
}

fn require(
    state: &AppState,
    headers: &HeaderMap,
    resource: &str,
    verb: &str,
) -> ApiResult<AuthContext> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?;
    let auth = lock(state)?
        .authenticate(value)
        .map_err(|_| unauthorized())?;
    if !kas_auth::allows(&auth.rules, resource, verb) {
        return Err(forbidden());
    }
    Ok(auth)
}

fn require_driver_ownership(auth: &AuthContext, driver_id: Uuid, generation: u64) -> ApiResult<()> {
    if let Some(auth_driver_id) = auth.driver_id {
        if auth_driver_id != driver_id || auth.driver_generation != Some(generation) {
            return Err(forbidden());
        }
    }
    Ok(())
}

fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "authentication required".into())
}

fn forbidden() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, "permission denied".into())
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
