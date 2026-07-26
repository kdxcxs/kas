use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocket},
        DefaultBodyLimit, Query, Request, State, WebSocketUpgrade,
    },
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use kas_auth::{AuthContext, IssuedCredential};
use kas_core::{
    package_path_for_digest, DriverControlState, DriverReady, DriverSpec, DriverState, DriverWork,
    Mutation, PackageSpec, Resource, RestartPolicy, UpdateResource, BUILTIN_PACKAGE_MEDIA_TYPE,
    MANIFEST_PACKAGE_MEDIA_TYPE,
};
use kas_driver::{ClientMessage, MutationError, MutationStatus, ServerMessage};
use kas_store::{Store, StoreError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

mod package;
mod supervisor;

use supervisor::{DriverLaunch, Supervisor};

const DRIVER_MANIFEST: &str = "/builtin/driver";
const PACKAGE_MANIFEST: &str = "/builtin/package";

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Store>>,
    driver_connections: Arc<Mutex<HashMap<String, Uuid>>>,
    data_dir: PathBuf,
    supervisor: Supervisor,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub data_dir: PathBuf,
    pub api_url: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            data_dir: std::env::var_os("KAS_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".data")),
            api_url: std::env::var("KAS_API_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:3000".into()),
        }
    }
}

pub fn app(store: Store) -> Router {
    app_with_config(store, AppConfig::default())
}

pub fn app_with_config(store: Store, config: AppConfig) -> Router {
    let store = Arc::new(Mutex::new(store));
    let supervisor = Supervisor::spawn(
        store.clone(),
        config.api_url.clone(),
        config.data_dir.clone(),
    );
    let state = AppState {
        store,
        driver_connections: Arc::new(Mutex::new(HashMap::new())),
        data_dir: config.data_dir,
        supervisor,
    };
    recover_drivers(&state);
    let protected = Router::new()
        .route("/resources", get(list_resources).post(create_resource))
        .route(
            "/resources/by-path",
            get(get_resource)
                .patch(update_resource)
                .delete(delete_resource),
        )
        .route(
            "/packages",
            post(install_package).layer(DefaultBodyLimit::max(256 * 1024 * 1024)),
        )
        .route("/drivers/control", post(control_driver))
        .route("/drivers/credentials", post(issue_driver_credential))
        .route("/drivers/connect", get(connect_driver))
        .route("/credentials/issue", post(issue_credential))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate_request,
        ));
    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

fn recover_drivers(state: &AppState) {
    let drivers = match lock(state).and_then(|store| store.list_drivers().map_err(Into::into)) {
        Ok(drivers) => drivers,
        Err(error) => {
            eprintln!("Driver recovery could not list Drivers: {}", error.1);
            return;
        }
    };
    for driver in drivers {
        let Ok(current_state) = decode_driver_state(&driver) else {
            eprintln!("Driver {} has invalid status", driver.path);
            continue;
        };
        if driver.metadata.state != "running" {
            continue;
        }
        match driver_launch(state, driver, None) {
            Ok(launch) => {
                let restart = decode_driver_spec(&launch.driver)
                    .map(|spec| spec.restart)
                    .unwrap_or(RestartPolicy::Never);
                if current_state == DriverState::Failed && restart == RestartPolicy::Never {
                    continue;
                }
                if let Err(error) = state.supervisor.ensure_running(launch) {
                    eprintln!("Driver recovery failed: {error:#}");
                }
            }
            Err(error) => eprintln!("Driver recovery failed: {}", error.1),
        }
    }
}

fn driver_launch(
    state: &AppState,
    driver: Resource,
    prepared_generation: Option<u64>,
) -> ApiResult<DriverLaunch> {
    let definition = decode_driver_spec(&driver)?;
    let manifest = lock(state)?.manifest_for_driver(&driver.path)?;
    let package = lock(state)?.package_for_driver(&driver.path)?;
    let package_spec: PackageSpec =
        serde_json::from_value(package.spec.clone()).map_err(internal_error)?;
    let package_root = if package_spec.media_type == BUILTIN_PACKAGE_MEDIA_TYPE {
        std::env::current_exe()
            .map_err(internal_error)?
            .parent()
            .ok_or_else(|| internal_error("KAS executable has no parent directory"))?
            .to_owned()
    } else {
        let hex = package_spec
            .digest
            .strip_prefix("sha256:")
            .ok_or_else(|| internal_error("Package digest is invalid"))?;
        state.data_dir.join("packages").join("sha256").join(hex)
    };
    let entrypoint = definition
        .entrypoint
        .strip_prefix("./")
        .unwrap_or(&definition.entrypoint);
    if !package_root.is_dir() || !package_root.join(entrypoint).is_file() {
        return Err(internal_error(format!(
            "Driver {} package or entrypoint is missing",
            driver.path
        )));
    }
    Ok(DriverLaunch {
        manifest_path: manifest.path.clone(),
        package_root,
        driver,
        prepared_generation,
    })
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true}))
}

async fn authenticate_request(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    match authenticate(&state, request.headers()) {
        Ok(_) => next.run(request).await,
        Err(error) => error.into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct ResourceListQuery {
    manifest: Option<String>,
}

async fn list_resources(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ResourceListQuery>,
) -> ApiResult<Json<Vec<Resource>>> {
    let auth = authenticate(&state, &headers)?;
    let resources = lock(&state)?.list_resources(query.manifest.as_deref())?;
    Ok(Json(
        resources
            .into_iter()
            .filter(|resource| {
                kas_auth::allows(
                    &auth.rules,
                    &resource.manifest,
                    "list",
                    Some(&resource.path),
                )
            })
            .collect(),
    ))
}

async fn create_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<kas_core::CreateResource>,
) -> ApiResult<(StatusCode, Json<Resource>)> {
    require(
        &state,
        &headers,
        &input.manifest,
        "create",
        Some(&input.path),
    )?;
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.create_resource(input)?),
    ))
}

#[derive(Debug, Deserialize)]
struct ObjectPathQuery {
    path: String,
}

async fn get_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<Json<Resource>> {
    let resource = lock(&state)?.get_resource(&query.path)?;
    require(
        &state,
        &headers,
        &resource.manifest,
        "get",
        Some(&resource.path),
    )?;
    Ok(Json(resource))
}

async fn update_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
    Json(input): Json<UpdateResource>,
) -> ApiResult<Json<Resource>> {
    let current = lock(&state)?.get_resource(&query.path)?;
    require(
        &state,
        &headers,
        &current.manifest,
        "update",
        Some(&current.path),
    )?;
    Ok(Json(lock(&state)?.update_resource(&query.path, input)?))
}

#[derive(Debug, Deserialize)]
struct DeleteResourceQuery {
    path: String,
    expected_revision: u64,
}

async fn delete_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DeleteResourceQuery>,
) -> ApiResult<Json<Resource>> {
    let current = lock(&state)?.get_resource(&query.path)?;
    require(
        &state,
        &headers,
        &current.manifest,
        "delete",
        Some(&current.path),
    )?;
    Ok(Json(
        lock(&state)?.delete_resource(&query.path, query.expected_revision)?,
    ))
}

async fn install_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Resource>)> {
    let expansion = package::inspect(&body).map_err(|error| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("Invalid Manifest package: {error:#}"),
        )
    })?;
    let auth = authenticate(&state, &headers)?;
    let package_path = package_path_for_digest(&expansion.artifact_digest)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "Invalid Package digest".into()))?;
    if !kas_auth::allows(&auth.rules, PACKAGE_MANIFEST, "create", Some(&package_path)) {
        return Err(forbidden());
    }
    for resource in &expansion.resources {
        if !kas_auth::allows(
            &auth.rules,
            &resource.manifest,
            "create",
            Some(&resource.path),
        ) {
            return Err(forbidden());
        }
    }
    let installed = package::install(&state.data_dir, &body).map_err(|error| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("Invalid Manifest package: {error:#}"),
        )
    })?;
    let driver_path = installed
        .expansion
        .resources
        .iter()
        .find(|resource| resource.manifest == DRIVER_MANIFEST)
        .map(|resource| resource.path.clone());
    let package = lock(&state)?.install_package(
        installed.expansion,
        installed.size_bytes,
        MANIFEST_PACKAGE_MEDIA_TYPE,
    )?;
    if let Some(driver_path) = driver_path {
        let driver = lock(&state)?.get_driver(&driver_path)?;
        if driver.metadata.state == "running" {
            let launch = driver_launch(&state, driver, None)?;
            state
                .supervisor
                .ensure_running(launch)
                .map_err(internal_error)?;
        }
    }
    Ok((StatusCode::CREATED, Json(package)))
}

#[derive(Debug, Deserialize)]
struct DriverControl {
    path: String,
    state: DriverControlState,
}

async fn control_driver(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<DriverControl>,
) -> ApiResult<Json<Resource>> {
    require(
        &state,
        &headers,
        DRIVER_MANIFEST,
        "update",
        Some(&input.path),
    )?;
    match input.state {
        DriverControlState::Running => {
            let driver = lock(&state)?.start_driver(&input.path)?;
            let generation = lock(&state)?.driver_generation(&input.path)?;
            let launch = driver_launch(&state, driver.clone(), Some(generation))?;
            state
                .supervisor
                .ensure_running(launch)
                .map_err(internal_error)?;
            Ok(Json(driver))
        }
        DriverControlState::Stopped => {
            let driver = lock(&state)?.stop_driver(&input.path)?;
            state.supervisor.stop(input.path).map_err(internal_error)?;
            Ok(Json(driver))
        }
    }
}

#[derive(Debug, Deserialize)]
struct IssueDriverCredential {
    path: String,
}

async fn issue_driver_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<IssueDriverCredential>,
) -> ApiResult<(StatusCode, Json<IssuedCredential>)> {
    require(
        &state,
        &headers,
        DRIVER_MANIFEST,
        "update",
        Some(&input.path),
    )?;
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.issue_driver_credential(&input.path)?),
    ))
}

#[derive(Debug, Deserialize)]
struct IssueCredential {
    subject: String,
    expires_at: Option<DateTime<Utc>>,
}

async fn issue_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<IssueCredential>,
) -> ApiResult<(StatusCode, Json<IssuedCredential>)> {
    let subject = lock(&state)?.get_resource(&input.subject)?;
    require(
        &state,
        &headers,
        &subject.manifest,
        "update",
        Some(&subject.path),
    )?;
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.issue_credential(&input.subject, input.expires_at)?),
    ))
}

#[derive(Debug, Deserialize)]
struct DriverConnectQuery {
    path: String,
    generation: u64,
}

async fn connect_driver(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DriverConnectQuery>,
) -> ApiResult<Response> {
    let auth = authenticate(&state, &headers)?;
    require_bound_driver(&auth, &query.path, query.generation)?;
    let driver = lock(&state)?.get_driver(&query.path)?;
    let current_generation = lock(&state)?.driver_generation(&query.path)?;
    let current_state = decode_driver_state(&driver)?;
    if current_generation != query.generation
        || !matches!(
            current_state,
            DriverState::Starting | DriverState::Running | DriverState::Stopping
        )
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Driver generation is stale or not connectable".into(),
        ));
    }
    let connection_id = Uuid::new_v4();
    state
        .driver_connections
        .lock()
        .map_err(|_| internal_error("connection lock poisoned"))?
        .insert(query.path.clone(), connection_id);
    Ok(ws
        .on_upgrade(move |socket| {
            serve_driver_socket(
                state,
                auth,
                query.path,
                query.generation,
                connection_id,
                socket,
            )
        })
        .into_response())
}

async fn serve_driver_socket(
    state: AppState,
    auth: AuthContext,
    driver_path: String,
    generation: u64,
    connection_id: Uuid,
    mut socket: WebSocket,
) {
    let control_delivery = Uuid::new_v4();
    let initial_driver =
        match lock(&state).and_then(|store| store.get_driver(&driver_path).map_err(Into::into)) {
            Ok(driver) => driver,
            Err(_) => return,
        };
    if send_server_message(
        &mut socket,
        &ServerMessage::Hello {
            delivery_id: control_delivery,
            driver: initial_driver,
        },
    )
    .await
    .is_err()
    {
        return;
    }

    let mut interval = tokio::time::interval(Duration::from_millis(25));
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.tick().await;
    let mut in_flight = None;
    let mut stop_delivery = None;
    let mut stop_acked = false;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if !is_current_connection(&state, &driver_path, connection_id) {
                    break;
                }
                let driver = match lock(&state).and_then(|store| store.get_driver(&driver_path).map_err(Into::into)) {
                    Ok(driver) => driver,
                    Err(_) => break,
                };
                let current_state = match decode_driver_state(&driver) {
                    Ok(driver_state)
                        if lock(&state)
                            .and_then(|store| store.driver_generation(&driver_path).map_err(Into::into))
                            .is_ok_and(|current| current == generation) => driver_state,
                    _ => break,
                };
                match current_state {
                    DriverState::Running if in_flight.is_none() => {
                        let delivery = match lock(&state).and_then(|mut store| {
                            store.claim_driver_delivery(&driver_path, generation).map_err(Into::into)
                        }) {
                            Ok(delivery) => delivery,
                            Err(error) => {
                                eprintln!("Driver {driver_path} delivery claim failed: {}", error.1);
                                break;
                            }
                        };
                        if let Some(delivery) = delivery {
                            let message = match delivery.work {
                                DriverWork::Reconcile { resource, .. } => ServerMessage::Reconcile {
                                    delivery_id: delivery.id,
                                    resource,
                                },
                                DriverWork::Run { run, resource, action } => ServerMessage::Run {
                                    delivery_id: delivery.id,
                                    run,
                                    resource,
                                    action,
                                },
                            };
                            if send_server_message(&mut socket, &message).await.is_err() {
                                break;
                            }
                            in_flight = Some(delivery.id);
                        }
                    }
                    DriverState::Stopping if stop_delivery.is_none() => {
                        let delivery_id = Uuid::new_v4();
                        if send_server_message(
                            &mut socket,
                            &ServerMessage::Stop { delivery_id, generation },
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                        stop_delivery = Some(delivery_id);
                    }
                    DriverState::Stopped | DriverState::Failed => break,
                    _ => {}
                }
            }
            _ = ping.tick() => {
                if send_server_message(&mut socket, &ServerMessage::Ping).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break; };
                let Message::Text(text) = message else {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                    continue;
                };
                let message = match serde_json::from_str::<ClientMessage>(text.as_str()) {
                    Ok(message) => message,
                    Err(error) => {
                        let _ = send_server_message(
                            &mut socket,
                            &ServerMessage::Error {
                                code: "invalid_message".into(),
                                message: error.to_string(),
                            },
                        )
                        .await;
                        continue;
                    }
                };
                let context = DriverMessageContext {
                    state: &state,
                    auth: &auth,
                    driver_path: &driver_path,
                    generation,
                    control_delivery,
                    stop_delivery,
                };
                match handle_driver_message(context, &mut in_flight, &mut stop_acked, message) {
                    Ok(Some(response)) => {
                        if send_server_message(&mut socket, &response).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("Driver {driver_path} WebSocket message failed: {}", error.1);
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        }
    }
    if let Ok(mut connections) = state.driver_connections.lock() {
        if connections.get(&driver_path) == Some(&connection_id) {
            connections.remove(&driver_path);
        }
    }
}

struct DriverMessageContext<'a> {
    state: &'a AppState,
    auth: &'a AuthContext,
    driver_path: &'a str,
    generation: u64,
    control_delivery: Uuid,
    stop_delivery: Option<Uuid>,
}

fn handle_driver_message(
    context: DriverMessageContext<'_>,
    in_flight: &mut Option<Uuid>,
    stop_acked: &mut bool,
    message: ClientMessage,
) -> ApiResult<Option<ServerMessage>> {
    let DriverMessageContext {
        state,
        auth,
        driver_path,
        generation,
        control_delivery,
        stop_delivery,
    } = context;
    match message {
        ClientMessage::Ready {
            generation: ready_generation,
            process_id,
            metadata,
        } => {
            require_bound_driver(auth, driver_path, ready_generation)?;
            if ready_generation != generation {
                return Err(forbidden());
            }
            let driver = lock(state)?.get_driver(driver_path)?;
            let current_state = decode_driver_state(&driver)?;
            if current_state == DriverState::Starting {
                lock(state)?.mark_driver_ready(
                    driver_path,
                    DriverReady {
                        generation,
                        process_id,
                        metadata,
                    },
                )?;
            } else if current_state == DriverState::Running {
                lock(state)?.heartbeat_driver(driver_path, generation)?;
            } else {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "Driver is not ready to connect".into(),
                ));
            }
        }
        ClientMessage::Ack { delivery_id } => {
            if Some(delivery_id) == stop_delivery {
                *stop_acked = true;
            } else if delivery_id != control_delivery {
                ensure_in_flight(*in_flight, delivery_id)?;
                lock(state)?.acknowledge_driver_delivery(delivery_id, driver_path, generation)?;
            }
        }
        ClientMessage::Mutation {
            request_id,
            delivery_id,
            driver_generation,
            operations,
        } => {
            let outcome = apply_driver_mutation(
                state,
                auth,
                driver_path,
                generation,
                *in_flight,
                request_id,
                delivery_id,
                driver_generation,
                operations,
            );
            let (status, results, error) = match outcome {
                Ok(results) => {
                    *in_flight = None;
                    (MutationStatus::Committed, results, None)
                }
                Err(error) => (
                    MutationStatus::Rejected,
                    Vec::new(),
                    Some(mutation_error(error)),
                ),
            };
            return Ok(Some(ServerMessage::MutationResult {
                request_id,
                delivery_id,
                status,
                results,
                error,
            }));
        }
        ClientMessage::Stopped {
            generation: stopped_generation,
        } => {
            require_bound_driver(auth, driver_path, stopped_generation)?;
            if stopped_generation != generation || stop_delivery.is_none() || !*stop_acked {
                return Err(forbidden());
            }
            lock(state)?.mark_driver_stopped(driver_path, generation)?;
        }
        ClientMessage::Pong => {
            lock(state)?.heartbeat_driver(driver_path, generation)?;
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn apply_driver_mutation(
    state: &AppState,
    auth: &AuthContext,
    driver_path: &str,
    generation: u64,
    in_flight: Option<Uuid>,
    request_id: Uuid,
    delivery_id: Uuid,
    driver_generation: u64,
    operations: Vec<Mutation>,
) -> ApiResult<Vec<Value>> {
    require_bound_driver(auth, driver_path, driver_generation)?;
    if request_id != delivery_id || driver_generation != generation {
        return Err(forbidden());
    }
    ensure_in_flight(in_flight, delivery_id)?;
    let delivery = lock(state)?.get_driver_delivery(delivery_id)?;
    if delivery.driver_path != driver_path || delivery.generation != generation {
        return Err(forbidden());
    }
    match delivery.work {
        DriverWork::Reconcile { resource, .. } => {
            for operation in &operations {
                match operation {
                    Mutation::UpdateResourceStatus { resource_path, .. }
                        if resource_path == &resource.path => {}
                    _ => authorize_mutation(state, auth, operation)?,
                }
            }
            Ok(lock(state)?.finish_reconciliation_with_mutations(
                delivery_id,
                driver_path,
                generation,
                operations,
            )?)
        }
        DriverWork::Run { run, .. } => {
            let Some((completion, mutations)) = operations.split_last() else {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "A Run mutation cannot be empty".into(),
                ));
            };
            let Mutation::CompleteRun {
                run_path,
                result: _,
            } = completion
            else {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "A Run mutation must end with complete_run".into(),
                ));
            };
            if run_path != &run.path {
                return Err(forbidden());
            }
            for mutation in mutations {
                authorize_mutation(state, auth, mutation)?;
            }
            lock(state)?
                .finish_run_delivery_with_mutations(
                    delivery_id,
                    driver_path,
                    generation,
                    operations,
                )
                .map_err(Into::into)
        }
    }
}

fn authorize_mutation(state: &AppState, auth: &AuthContext, mutation: &Mutation) -> ApiResult<()> {
    let (manifest, verb, path) = match mutation {
        Mutation::CreateResource { resource } => {
            (&resource.manifest, "create", resource.path.as_str())
        }
        Mutation::UpdateResource { resource_path, .. }
        | Mutation::UpdateResourceStatus { resource_path, .. } => {
            let resource = lock(state)?.get_resource(resource_path)?;
            if !kas_auth::allows(
                &auth.rules,
                &resource.manifest,
                "update",
                Some(resource_path),
            ) {
                return Err(forbidden());
            }
            return Ok(());
        }
        Mutation::DeleteResource { resource_path, .. } => {
            let resource = lock(state)?.get_resource(resource_path)?;
            if !kas_auth::allows(
                &auth.rules,
                &resource.manifest,
                "delete",
                Some(resource_path),
            ) {
                return Err(forbidden());
            }
            return Ok(());
        }
        Mutation::CompleteRun { .. } => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "complete_run is only valid as the final Run delivery operation".into(),
            ));
        }
    };
    if kas_auth::allows(&auth.rules, manifest, verb, Some(path)) {
        Ok(())
    } else {
        Err(forbidden())
    }
}

fn mutation_error(error: ApiError) -> MutationError {
    let code = match error.0 {
        StatusCode::BAD_REQUEST => "invalid_request",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "conflict",
        _ => "internal_error",
    };
    MutationError {
        code: code.into(),
        message: error.1,
    }
}

fn ensure_in_flight(in_flight: Option<Uuid>, delivery_id: Uuid) -> ApiResult<()> {
    if in_flight == Some(delivery_id) {
        Ok(())
    } else {
        Err(forbidden())
    }
}

fn is_current_connection(state: &AppState, driver_path: &str, connection_id: Uuid) -> bool {
    state
        .driver_connections
        .lock()
        .ok()
        .and_then(|connections| connections.get(driver_path).copied())
        == Some(connection_id)
}

async fn send_server_message(socket: &mut WebSocket, message: &ServerMessage) -> Result<(), ()> {
    socket
        .send(Message::Text(
            serde_json::to_string(message).map_err(|_| ())?.into(),
        ))
        .await
        .map_err(|_| ())
}

fn decode_driver_spec(driver: &Resource) -> ApiResult<DriverSpec> {
    serde_json::from_value(driver.spec.clone()).map_err(|error| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Driver {} has invalid spec: {error}", driver.path),
        )
    })
}

fn decode_driver_state(driver: &Resource) -> ApiResult<DriverState> {
    serde_json::from_value(serde_json::Value::String(
        driver.status.metadata.state.clone(),
    ))
    .map_err(internal_error)
}

fn require(
    state: &AppState,
    headers: &HeaderMap,
    manifest: &str,
    verb: &str,
    path: Option<&str>,
) -> ApiResult<AuthContext> {
    let auth = authenticate(state, headers)?;
    if !kas_auth::allows(&auth.rules, manifest, verb, path) {
        return Err(forbidden());
    }
    Ok(auth)
}

fn authenticate(state: &AppState, headers: &HeaderMap) -> ApiResult<AuthContext> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?;
    lock(state)?.authenticate(value).map_err(|_| unauthorized())
}

fn require_bound_driver(auth: &AuthContext, driver_path: &str, generation: u64) -> ApiResult<()> {
    if auth.driver_path.as_deref() != Some(driver_path)
        || auth.driver_generation != Some(generation)
    {
        return Err(forbidden());
    }
    Ok(())
}

fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "authentication required".into())
}

fn forbidden() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, "permission denied".into())
}

fn internal_error(error: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn lock(state: &AppState) -> Result<std::sync::MutexGuard<'_, Store>, ApiError> {
    state
        .store
        .lock()
        .map_err(|_| internal_error("store lock poisoned"))
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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
