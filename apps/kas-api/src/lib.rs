use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Query, Request, State, WebSocketUpgrade,
    },
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use kas_auth::{
    AuthContext, CreateRole, CreateRoleBinding, CreateServiceAccount, CreateUser, IssuedCredential,
    Role, RoleBinding, Rule, ServiceAccount, User,
};
use kas_core::{
    CreateLink, CreateManifest, CreateResource, CreateRun, Driver, DriverReady, DriverWork, Event,
    EventFilter, EventType, FinishRun, Link, LinkFilter, Manifest, Mutation, ObjectKind, ObjectRef,
    Resource, Run, UpdateResource, UpdateResourceStatus,
};
use kas_driver::{
    ClientMessage, MutationError, MutationStatus, ServerMessage, WatchObject, WatchSelector,
};
use kas_store::{Store, StoreError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Store>>,
    driver_connections: Arc<Mutex<HashMap<String, Uuid>>>,
}

pub fn app(store: Store) -> Router {
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        driver_connections: Arc::new(Mutex::new(HashMap::new())),
    };
    let protected = Router::new()
        .route("/manifests", get(list_manifests).post(create_manifest))
        .route("/manifests/driver", get(get_manifest_driver))
        .route("/resources", get(list_resources).post(create_resource))
        .route(
            "/resources/by-path",
            get(get_resource).patch(update_resource_spec),
        )
        .route(
            "/resources/status",
            axum::routing::put(update_resource_status),
        )
        .route("/drivers/by-path", get(get_driver).patch(update_driver))
        .route("/drivers/claim", post(claim_work))
        .route("/drivers/connect", get(connect_driver))
        .route("/drivers/credentials", post(issue_driver_credential))
        .route("/runs", post(enqueue_run))
        .route("/runs/by-path", get(get_run))
        .route("/runs/result", axum::routing::put(finish_run))
        .route("/links", get(list_links).post(create_link))
        .route("/links/by-path", get(get_link).delete(delete_link))
        .route("/users", get(list_users).post(create_user))
        .route("/users/credentials", post(issue_user_credential))
        .route(
            "/service-accounts",
            get(list_service_accounts).post(create_service_account),
        )
        .route(
            "/service-accounts/credentials",
            post(issue_service_account_credential),
        )
        .route("/roles", get(list_roles).post(create_role))
        .route(
            "/roles/by-path",
            axum::routing::patch(update_role).delete(delete_role),
        )
        .route(
            "/role-bindings",
            get(list_role_bindings).post(create_role_binding),
        )
        .route(
            "/role-bindings/by-path",
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

struct ActiveWatch {
    cursor: u64,
    selectors: Vec<WatchSelector>,
}

#[derive(Debug, Deserialize)]
struct ObjectPathQuery {
    path: String,
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
    require(&state, &headers, "manifests", "create", Some(&input.path))?;
    let manifest = lock(&state)?.create_manifest(input)?;
    Ok((StatusCode::CREATED, Json(manifest)))
}

async fn list_manifests(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Manifest>>> {
    let auth = authenticate(&state, &headers)?;
    let manifests = lock(&state)?
        .list_manifests()?
        .into_iter()
        .filter(|manifest| kas_auth::allows(&auth.rules, "manifests", "list", Some(&manifest.path)))
        .collect();
    Ok(Json(manifests))
}

async fn get_manifest_driver(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<Json<Option<Driver>>> {
    require(&state, &headers, "manifests", "get", Some(&query.path))?;
    Ok(Json(lock(&state)?.driver_for_manifest(&query.path)?))
}

async fn create_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateResource>,
) -> ApiResult<(StatusCode, Json<Resource>)> {
    let manifest = lock(&state)?.get_manifest(&input.manifest_path)?;
    require(
        &state,
        &headers,
        &format!("resources/{}", manifest.name),
        "create",
        Some(&input.path),
    )?;
    let resource = lock(&state)?.create_resource(input)?;
    Ok((StatusCode::CREATED, Json(resource)))
}

async fn list_resources(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Resource>>> {
    let auth = authenticate(&state, &headers)?;
    let store = lock(&state)?;
    let resources = store
        .list_resources()?
        .into_iter()
        .filter(|resource| {
            store
                .get_manifest(&resource.manifest_path)
                .is_ok_and(|manifest| {
                    kas_auth::allows(
                        &auth.rules,
                        &format!("resources/{}", manifest.name),
                        "list",
                        Some(&resource.path),
                    )
                })
        })
        .collect();
    Ok(Json(resources))
}

async fn get_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<Json<Resource>> {
    let resource = lock(&state)?.get_resource(&query.path)?;
    let manifest = lock(&state)?.get_manifest(&resource.manifest_path)?;
    require(
        &state,
        &headers,
        &format!("resources/{}", manifest.name),
        "get",
        Some(&resource.path),
    )?;
    Ok(Json(resource))
}

async fn update_resource_spec(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
    Json(input): Json<UpdateResource>,
) -> ApiResult<Json<Resource>> {
    let resource = lock(&state)?.get_resource(&query.path)?;
    let manifest = lock(&state)?.get_manifest(&resource.manifest_path)?;
    require(
        &state,
        &headers,
        &format!("resources/{}", manifest.name),
        "patch",
        Some(&resource.path),
    )?;
    Ok(Json(lock(&state)?.update_resource(&query.path, input)?))
}

async fn update_resource_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
    Json(input): Json<UpdateResourceStatus>,
) -> ApiResult<Json<Resource>> {
    let auth = require(
        &state,
        &headers,
        "resources/status",
        "update",
        Some(&query.path),
    )?;
    require_driver_ownership(&auth, &input.driver_path, input.driver_generation)?;
    Ok(Json(
        lock(&state)?.update_resource_status(&query.path, input)?,
    ))
}

async fn get_driver(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<Json<Driver>> {
    let auth = require(&state, &headers, "drivers", "get", Some(&query.path))?;
    if let Some(driver_path) = auth.driver_path.as_deref() {
        if driver_path != query.path {
            return Err(forbidden());
        }
    }
    Ok(Json(lock(&state)?.get_driver(&query.path)?))
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
    Query(query): Query<ObjectPathQuery>,
    Json(input): Json<DriverUpdate>,
) -> ApiResult<Json<Driver>> {
    let auth = require(&state, &headers, "drivers", "patch", Some(&query.path))?;
    if let Some(driver_path) = auth.driver_path.as_deref() {
        if driver_path != query.path {
            return Err(forbidden());
        }
        let generation = match &input {
            DriverUpdate::Ready { generation, .. } | DriverUpdate::Stopped { generation } => {
                *generation
            }
            DriverUpdate::Starting | DriverUpdate::Stopping => return Err(forbidden()),
        };
        require_driver_ownership(&auth, &query.path, generation)?;
    }
    let driver = match input {
        DriverUpdate::Starting => lock(&state)?.start_driver(&query.path)?,
        DriverUpdate::Ready {
            generation,
            process_id,
            metadata,
        } => lock(&state)?.mark_driver_ready(
            &query.path,
            DriverReady {
                generation,
                process_id,
                metadata,
            },
        )?,
        DriverUpdate::Stopping => lock(&state)?.stop_driver(&query.path)?,
        DriverUpdate::Stopped { generation } => {
            lock(&state)?.mark_driver_stopped(&query.path, generation)?
        }
    };
    Ok(Json(driver))
}

async fn enqueue_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateRun>,
) -> ApiResult<(StatusCode, Json<Run>)> {
    require(&state, &headers, "runs", "create", Some(&input.path))?;
    let run = lock(&state)?.enqueue_run(input)?;
    Ok((StatusCode::ACCEPTED, Json(run)))
}

async fn get_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<Json<Run>> {
    require(&state, &headers, "runs", "get", Some(&query.path))?;
    Ok(Json(lock(&state)?.get_run(&query.path)?))
}

async fn claim_work(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
    Json(input): Json<ClaimWork>,
) -> ApiResult<Json<Option<DriverWork>>> {
    let auth = require(
        &state,
        &headers,
        "drivers/claim",
        "create",
        Some(&query.path),
    )?;
    require_driver_ownership(&auth, &query.path, input.generation)?;
    Ok(Json(
        lock(&state)?.claim_driver_work(&query.path, input.generation)?,
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
    let auth = require(
        &state,
        &headers,
        "drivers/connect",
        "create",
        Some(&query.path),
    )?;
    require_driver_ownership(&auth, &query.path, query.generation)?;
    let driver = lock(&state)?.get_driver(&query.path)?;
    if driver.generation != query.generation
        || !matches!(
            driver.state,
            kas_core::DriverState::Starting
                | kas_core::DriverState::Ready
                | kas_core::DriverState::Stopping
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
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "connection lock poisoned".into(),
            )
        })?
        .insert(query.path.clone(), connection_id);
    let driver_path = query.path;
    let generation = query.generation;
    Ok(ws
        .on_upgrade(move |socket| {
            serve_driver_socket(state, auth, driver_path, generation, connection_id, socket)
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
    let cursor =
        match lock(&state).and_then(|store| store.current_event_cursor().map_err(ApiError::from)) {
            Ok(cursor) => cursor,
            Err(_) => return,
        };
    if send_server_message(
        &mut socket,
        &ServerMessage::Hello {
            delivery_id: control_delivery,
            driver: initial_driver,
            cursor,
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
    let mut in_flight: Option<Uuid> = None;
    let mut stop_delivery: Option<Uuid> = None;
    let mut watches: HashMap<Uuid, ActiveWatch> = HashMap::new();
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if !is_current_connection(&state, &driver_path, connection_id) {
                    break;
                }
                if push_watch_events(&state, &auth, &mut socket, &mut watches).await.is_err() {
                    break;
                }
                let driver = match lock(&state).and_then(|store| store.get_driver(&driver_path).map_err(Into::into)) {
                    Ok(driver) if driver.generation == generation => driver,
                    _ => break,
                };
                match driver.state {
                    kas_core::DriverState::Ready if in_flight.is_none() => {
                        let delivery = match lock(&state).and_then(|mut store| store.claim_driver_delivery(&driver_path, generation).map_err(Into::into)) {
                            Ok(delivery) => delivery,
                            Err(error) => {
                                eprintln!("Driver {driver_path} delivery claim failed: {}", error.1);
                                break;
                            }
                        };
                        if let Some(delivery) = delivery {
                            let message = match delivery.work {
                                DriverWork::Reconcile { resource, revision } => ServerMessage::Reconcile {
                                    delivery_id: delivery.id,
                                    resource,
                                    revision,
                                },
                                DriverWork::Run { run, resource } => ServerMessage::Run {
                                    delivery_id: delivery.id,
                                    run,
                                    resource,
                                },
                            };
                            if send_server_message(&mut socket, &message).await.is_err() {
                                break;
                            }
                            in_flight = Some(delivery.id);
                        }
                    }
                    kas_core::DriverState::Stopping if stop_delivery.is_none() => {
                        let delivery_id = Uuid::new_v4();
                        if send_server_message(&mut socket, &ServerMessage::Stop { delivery_id, generation }).await.is_err() {
                            break;
                        }
                        stop_delivery = Some(delivery_id);
                    }
                    kas_core::DriverState::Stopped | kas_core::DriverState::Failed => break,
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
                    if matches!(message, Message::Close(_)) { break; }
                    continue;
                };
                let message = match serde_json::from_str::<ClientMessage>(text.as_str()) {
                    Ok(message) => message,
                    Err(error) => {
                        if send_server_message(
                            &mut socket,
                            &ServerMessage::Error {
                                request_id: None,
                                watch_id: None,
                                code: "invalid_message".into(),
                                message: error.to_string(),
                            },
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
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
                match handle_driver_message(context, &mut in_flight, &mut watches, message) {
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
    watches: &mut HashMap<Uuid, ActiveWatch>,
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
            require_driver_ownership(auth, driver_path, ready_generation)?;
            if ready_generation != generation {
                return Err(forbidden());
            }
            let driver = lock(state)?.get_driver(driver_path)?;
            if driver.state == kas_core::DriverState::Starting {
                lock(state)?.mark_driver_ready(
                    driver_path,
                    DriverReady {
                        generation,
                        process_id,
                        metadata,
                    },
                )?;
            } else if driver.state == kas_core::DriverState::Ready {
                lock(state)?.heartbeat_driver(driver_path, generation)?;
            } else {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "Driver is not ready to connect".into(),
                ));
            }
        }
        ClientMessage::Ack { delivery_id } => {
            if delivery_id != control_delivery && Some(delivery_id) != stop_delivery {
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
        ClientMessage::Watch {
            request_id,
            cursor,
            selectors,
        } => {
            if selectors.is_empty() {
                return Ok(Some(ServerMessage::Error {
                    request_id: Some(request_id),
                    watch_id: None,
                    code: "invalid_request".into(),
                    message: "A watch must contain at least one selector".into(),
                }));
            }
            if let Err(error) = authorize_watch(state, auth, &selectors) {
                return Ok(Some(watch_error_response(Some(request_id), None, error)));
            }
            let current_cursor = lock(state)?.current_event_cursor()?;
            let accepted_cursor = match cursor {
                Some(cursor) if cursor <= current_cursor => cursor,
                Some(_) => {
                    return Ok(Some(ServerMessage::Error {
                        request_id: Some(request_id),
                        watch_id: None,
                        code: "invalid_cursor".into(),
                        message: format!(
                            "Watch cursor cannot be greater than the current cursor {current_cursor}"
                        ),
                    }));
                }
                None => current_cursor,
            };
            let watch_id = Uuid::new_v4();
            watches.insert(
                watch_id,
                ActiveWatch {
                    cursor: accepted_cursor,
                    selectors,
                },
            );
            return Ok(Some(ServerMessage::WatchReady {
                request_id,
                watch_id,
                cursor: accepted_cursor,
            }));
        }
        ClientMessage::Unwatch { watch_id } => {
            if watches.remove(&watch_id).is_some() {
                return Ok(Some(ServerMessage::WatchClosed { watch_id }));
            }
            return Ok(Some(ServerMessage::Error {
                request_id: None,
                watch_id: Some(watch_id),
                code: "not_found".into(),
                message: "Watch not found".into(),
            }));
        }
        ClientMessage::Stopped {
            generation: stopped_generation,
        } => {
            require_driver_ownership(auth, driver_path, stopped_generation)?;
            if stopped_generation != generation {
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
    require_driver_ownership(auth, driver_path, driver_generation)?;
    if request_id != delivery_id || driver_generation != generation {
        return Err(forbidden());
    }
    ensure_in_flight(in_flight, delivery_id)?;
    let delivery = lock(state)?.get_driver_delivery(delivery_id)?;
    if delivery.driver_path != driver_path || delivery.generation != generation {
        return Err(forbidden());
    }
    match delivery.work {
        DriverWork::Reconcile { resource, revision } => {
            let [Mutation::UpdateResourceStatus {
                resource_path,
                observed_revision,
                status,
            }] = operations.as_slice()
            else {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "A reconciliation mutation must contain exactly one update_resource_status operation"
                        .into(),
                ));
            };
            if resource_path != &resource.path || *observed_revision != revision {
                return Err(forbidden());
            }
            if !kas_auth::allows(
                &auth.rules,
                "resources/status",
                "update",
                Some(resource_path),
            ) {
                return Err(forbidden());
            }
            let resource = lock(state)?.finish_reconciliation_delivery(
                delivery_id,
                driver_path,
                generation,
                resource_path,
                *observed_revision,
                status.clone(),
            )?;
            Ok(vec![serde_json::to_value(resource).map_err(|error| {
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            })?])
        }
        DriverWork::Run { run, .. } => {
            let Some((completion, mutations)) = operations.split_last() else {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "A Run mutation cannot be empty".into(),
                ));
            };
            let Mutation::CompleteRun { run_path, result } = completion else {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "A Run mutation must end with complete_run".into(),
                ));
            };
            if run_path != &run.path {
                return Err(forbidden());
            }
            authorize_mutations(state, auth, mutations)?;
            let run = lock(state)?.finish_run_delivery_with_mutations(
                delivery_id,
                driver_path,
                generation,
                run_path,
                FinishRun {
                    driver_generation,
                    result: result.clone(),
                },
                mutations.to_vec(),
            )?;
            Ok(vec![serde_json::to_value(run).map_err(|error| {
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            })?])
        }
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

fn authorize_mutations(
    state: &AppState,
    auth: &AuthContext,
    mutations: &[Mutation],
) -> ApiResult<()> {
    let mut pending_resources = HashMap::new();
    for mutation in mutations {
        let (resource_key, verb, path) = match mutation {
            Mutation::CreateResource { resource } => {
                let manifest = lock(state)?.get_manifest(&resource.manifest_path)?;
                pending_resources.insert(resource.path.clone(), manifest.name.clone());
                (
                    format!("resources/{}", manifest.name),
                    "create",
                    resource.path.as_str(),
                )
            }
            Mutation::UpdateResource { resource_path, .. } => {
                let resource = lock(state)?.get_resource(resource_path)?;
                let manifest = lock(state)?.get_manifest(&resource.manifest_path)?;
                (
                    format!("resources/{}", manifest.name),
                    "patch",
                    resource_path.as_str(),
                )
            }
            Mutation::CreateLink { link } => {
                authorize_link_endpoint(state, auth, &link.source, Some(&pending_resources))?;
                authorize_link_endpoint(state, auth, &link.target, Some(&pending_resources))?;
                ("links".into(), "create", link.path.as_str())
            }
            Mutation::UpdateResourceStatus { .. } | Mutation::CompleteRun { .. } => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "Lifecycle operations may only appear in their delivery-specific position"
                        .into(),
                ));
            }
        };
        if !kas_auth::allows(&auth.rules, &resource_key, verb, Some(path)) {
            return Err(forbidden());
        }
    }
    Ok(())
}

fn authorize_watch(
    state: &AppState,
    auth: &AuthContext,
    selectors: &[WatchSelector],
) -> ApiResult<()> {
    for selector in selectors {
        let (resource, path) = match selector {
            WatchSelector::Resource {
                manifest_path: Some(manifest_path),
                path,
            } => (
                format!("resources/{}", manifest_name(state, manifest_path)?),
                path,
            ),
            WatchSelector::Resource {
                manifest_path: None,
                path,
            } => ("resources/*".into(), path),
            WatchSelector::Link { path, .. } => ("links".into(), path),
            WatchSelector::Run { path, .. } => ("runs".into(), path),
        };
        let proposed = Rule {
            resources: vec![resource],
            verbs: vec!["watch".into()],
            paths: path.iter().cloned().collect(),
        };
        if !kas_auth::rules_are_subset(&[proposed], &auth.rules) {
            return Err(forbidden());
        }
    }
    Ok(())
}

async fn push_watch_events(
    state: &AppState,
    auth: &AuthContext,
    socket: &mut WebSocket,
    watches: &mut HashMap<Uuid, ActiveWatch>,
) -> Result<(), ()> {
    let watch_ids: Vec<_> = watches.keys().copied().collect();
    for watch_id in watch_ids {
        let Some(watch) = watches.get(&watch_id) else {
            continue;
        };
        let events = lock(state)
            .and_then(|store| {
                store
                    .list_events_filtered(EventFilter {
                        after_sequence: Some(watch.cursor),
                        limit: Some(100),
                        ..Default::default()
                    })
                    .map_err(Into::into)
            })
            .map_err(|_| ())?;
        for event in events {
            // A cursor represents the global event stream, not only matching
            // events. Advancing across non-matches prevents rescanning them.
            if let Some(watch) = watches.get_mut(&watch_id) {
                watch.cursor = event.sequence;
            }
            let matches = watches.get(&watch_id).is_some_and(|watch| {
                watch
                    .selectors
                    .iter()
                    .any(|selector| selector_matches(selector, &event))
            });
            if matches && event_is_authorized(state, auth, &event) {
                let message = watch_event_message(watch_id, event);
                send_server_message(socket, &message).await?;
            }
        }
    }
    Ok(())
}

fn selector_matches(selector: &WatchSelector, event: &Event) -> bool {
    match selector {
        WatchSelector::Resource {
            manifest_path,
            path,
        } => {
            event.object_kind == ObjectKind::Resource
                && manifest_path
                    .as_ref()
                    .is_none_or(|value| event.manifest_path.as_ref() == Some(value))
                && path
                    .as_ref()
                    .is_none_or(|pattern| kas_auth::path_matches(pattern, &event.object_path))
        }
        WatchSelector::Link {
            path,
            relation,
            source,
            target,
        } => {
            if event.object_kind != ObjectKind::Link {
                return false;
            }
            let Ok(link) = serde_json::from_value::<Link>(event.value.clone()) else {
                return false;
            };
            path.as_ref()
                .is_none_or(|pattern| kas_auth::path_matches(pattern, &event.object_path))
                && relation
                    .as_ref()
                    .is_none_or(|value| &link.relation == value)
                && source.as_ref().is_none_or(|value| &link.source == value)
                && target.as_ref().is_none_or(|value| &link.target == value)
        }
        WatchSelector::Run {
            resource_path,
            path,
        } => {
            if event.object_kind != ObjectKind::Run {
                return false;
            }
            path.as_ref()
                .is_none_or(|pattern| kas_auth::path_matches(pattern, &event.object_path))
                && resource_path.as_ref().is_none_or(|resource_path| {
                    serde_json::from_value::<Run>(event.value.clone())
                        .is_ok_and(|run| &run.resource_path == resource_path)
                })
        }
    }
}

fn event_is_authorized(state: &AppState, auth: &AuthContext, event: &Event) -> bool {
    let resource = match event.object_kind {
        ObjectKind::Resource => {
            let Some(manifest_path) = event.manifest_path.as_deref() else {
                return false;
            };
            let Ok(name) = manifest_name(state, manifest_path) else {
                return false;
            };
            format!("resources/{name}")
        }
        ObjectKind::Link => "links".into(),
        ObjectKind::Run => "runs".into(),
        ObjectKind::Manifest => "manifests".into(),
        ObjectKind::Driver => "drivers".into(),
        ObjectKind::User => "users".into(),
        ObjectKind::ServiceAccount => "serviceaccounts".into(),
        ObjectKind::Role => "roles".into(),
        ObjectKind::RoleBinding => "rolebindings".into(),
        ObjectKind::Credential => "credentials".into(),
    };
    kas_auth::allows(&auth.rules, &resource, "watch", Some(&event.object_path))
}

fn watch_event_message(watch_id: Uuid, event: Event) -> ServerMessage {
    let object = WatchObject {
        kind: event.object_kind,
        path: event.object_path,
        revision: event.revision,
        value: event.value,
    };
    match event.event_type {
        EventType::Created => ServerMessage::Created {
            watch_id,
            cursor: event.sequence,
            object,
        },
        EventType::Updated => ServerMessage::Updated {
            watch_id,
            cursor: event.sequence,
            object,
        },
        EventType::Deleted => ServerMessage::Deleted {
            watch_id,
            cursor: event.sequence,
            object,
        },
    }
}

fn watch_error_response(
    request_id: Option<Uuid>,
    watch_id: Option<Uuid>,
    error: ApiError,
) -> ServerMessage {
    let code = match error.0 {
        StatusCode::BAD_REQUEST => "invalid_request",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        _ => "internal_error",
    };
    ServerMessage::Error {
        request_id,
        watch_id,
        code: code.into(),
        message: error.1,
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

async fn finish_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
    Json(input): Json<FinishRun>,
) -> ApiResult<Json<Run>> {
    let auth = require(&state, &headers, "runs/result", "update", Some(&query.path))?;
    if let Some(driver_path) = auth.driver_path.as_deref() {
        require_driver_ownership(&auth, driver_path, input.driver_generation)?;
    }
    Ok(Json(lock(&state)?.finish_run(&query.path, input)?))
}

async fn issue_driver_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<(StatusCode, Json<IssuedCredential>)> {
    require(
        &state,
        &headers,
        "drivers/credentials",
        "create",
        Some(&query.path),
    )?;
    let credential = lock(&state)?.issue_driver_credential(&query.path)?;
    Ok((StatusCode::CREATED, Json(credential)))
}

async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateUser>,
) -> ApiResult<(StatusCode, Json<User>)> {
    require(&state, &headers, "users", "create", Some(&input.path))?;
    Ok((StatusCode::CREATED, Json(lock(&state)?.create_user(input)?)))
}

async fn list_users(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<User>>> {
    let auth = authenticate(&state, &headers)?;
    let users = lock(&state)?
        .list_users()?
        .into_iter()
        .filter(|user| kas_auth::allows(&auth.rules, "users", "list", Some(&user.path)))
        .collect();
    Ok(Json(users))
}

async fn issue_user_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<(StatusCode, Json<IssuedCredential>)> {
    require(&state, &headers, "credentials", "create", Some(&query.path))?;
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.issue_user_credential(&query.path)?),
    ))
}

async fn create_service_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateServiceAccount>,
) -> ApiResult<(StatusCode, Json<ServiceAccount>)> {
    require(
        &state,
        &headers,
        "serviceaccounts",
        "create",
        Some(&input.path),
    )?;
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.create_service_account(input)?),
    ))
}

async fn list_service_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<ServiceAccount>>> {
    let auth = authenticate(&state, &headers)?;
    let accounts = lock(&state)?
        .list_service_accounts()?
        .into_iter()
        .filter(|account| {
            kas_auth::allows(&auth.rules, "serviceaccounts", "list", Some(&account.path))
        })
        .collect();
    Ok(Json(accounts))
}

async fn issue_service_account_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<(StatusCode, Json<IssuedCredential>)> {
    require(&state, &headers, "credentials", "create", Some(&query.path))?;
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.issue_service_account_credential(&query.path)?),
    ))
}

async fn create_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateRole>,
) -> ApiResult<(StatusCode, Json<Role>)> {
    let auth = require(&state, &headers, "roles", "create", Some(&input.path))?;
    if !kas_auth::allows(&auth.rules, "roles", "escalate", Some(&input.path))
        && !kas_auth::rules_are_subset(&input.rules, &auth.rules)
    {
        return Err(forbidden());
    }
    Ok((StatusCode::CREATED, Json(lock(&state)?.create_role(input)?)))
}

async fn list_roles(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Role>>> {
    let auth = authenticate(&state, &headers)?;
    let roles = lock(&state)?
        .list_roles()?
        .into_iter()
        .filter(|role| kas_auth::allows(&auth.rules, "roles", "list", Some(&role.path)))
        .collect();
    Ok(Json(roles))
}

async fn update_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
    Json(input): Json<CreateRole>,
) -> ApiResult<Json<Role>> {
    if input.path != query.path {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Role path is immutable".into(),
        ));
    }
    let auth = require(&state, &headers, "roles", "patch", Some(&query.path))?;
    if !kas_auth::allows(&auth.rules, "roles", "escalate", Some(&query.path))
        && !kas_auth::rules_are_subset(&input.rules, &auth.rules)
    {
        return Err(forbidden());
    }
    Ok(Json(lock(&state)?.update_role(&query.path, input)?))
}

async fn delete_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<StatusCode> {
    require(&state, &headers, "roles", "delete", Some(&query.path))?;
    lock(&state)?.delete_role(&query.path)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_role_binding(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateRoleBinding>,
) -> ApiResult<(StatusCode, Json<RoleBinding>)> {
    let auth = require(
        &state,
        &headers,
        "rolebindings",
        "create",
        Some(&input.path),
    )?;
    let role = lock(&state)?.get_role(&input.role_path)?;
    if !kas_auth::allows(&auth.rules, "roles", "bind", Some(&role.path))
        && !kas_auth::rules_are_subset(&role.rules, &auth.rules)
    {
        return Err(forbidden());
    }
    Ok((
        StatusCode::CREATED,
        Json(lock(&state)?.create_role_binding(input)?),
    ))
}

async fn list_role_bindings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<RoleBinding>>> {
    let auth = authenticate(&state, &headers)?;
    let bindings = lock(&state)?
        .list_role_bindings()?
        .into_iter()
        .filter(|binding| {
            kas_auth::allows(&auth.rules, "rolebindings", "list", Some(&binding.path))
        })
        .collect();
    Ok(Json(bindings))
}

async fn delete_role_binding(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<StatusCode> {
    require(
        &state,
        &headers,
        "rolebindings",
        "delete",
        Some(&query.path),
    )?;
    lock(&state)?.delete_role_binding(&query.path)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateLink>,
) -> ApiResult<(StatusCode, Json<Link>)> {
    let auth = require(&state, &headers, "links", "create", Some(&input.path))?;
    authorize_link_endpoint(&state, &auth, &input.source, None)?;
    authorize_link_endpoint(&state, &auth, &input.target, None)?;
    Ok((StatusCode::CREATED, Json(lock(&state)?.create_link(input)?)))
}

fn authorize_link_endpoint(
    state: &AppState,
    auth: &AuthContext,
    object: &ObjectRef,
    pending_resources: Option<&HashMap<String, String>>,
) -> ApiResult<()> {
    let resource = match object.kind {
        ObjectKind::Manifest => "manifests".into(),
        ObjectKind::Resource => {
            let manifest_name = pending_resources
                .and_then(|resources| resources.get(&object.path))
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| {
                    let resource = lock(state)?.get_resource(&object.path)?;
                    manifest_name(state, &resource.manifest_path)
                })?;
            format!("resources/{manifest_name}")
        }
        ObjectKind::Driver => "drivers".into(),
        ObjectKind::Run => "runs".into(),
        ObjectKind::Link => "links".into(),
        ObjectKind::User => "users".into(),
        ObjectKind::ServiceAccount => "serviceaccounts".into(),
        ObjectKind::Role => "roles".into(),
        ObjectKind::RoleBinding => "rolebindings".into(),
        ObjectKind::Credential => "credentials".into(),
    };
    if !kas_auth::allows(&auth.rules, &resource, "link", Some(&object.path)) {
        return Err(forbidden());
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct LinkQuery {
    source_kind: Option<ObjectKind>,
    source_path: Option<String>,
    relation: Option<String>,
    target_kind: Option<ObjectKind>,
    target_path: Option<String>,
}

fn optional_object_ref(
    kind: Option<ObjectKind>,
    path: Option<String>,
    label: &str,
) -> ApiResult<Option<ObjectRef>> {
    match (kind, path) {
        (Some(kind), Some(path)) => Ok(Some(ObjectRef { kind, path })),
        (None, None) => Ok(None),
        _ => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("{label}_kind and {label}_path must be provided together"),
        )),
    }
}

async fn list_links(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LinkQuery>,
) -> ApiResult<Json<Vec<Link>>> {
    let auth = authenticate(&state, &headers)?;
    let filter = LinkFilter {
        source: optional_object_ref(query.source_kind, query.source_path, "source")?,
        relation: query.relation,
        target: optional_object_ref(query.target_kind, query.target_path, "target")?,
    };
    let links = lock(&state)?
        .list_links(filter)?
        .into_iter()
        .filter(|link| kas_auth::allows(&auth.rules, "links", "list", Some(&link.path)))
        .collect();
    Ok(Json(links))
}

async fn get_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<Json<Link>> {
    require(&state, &headers, "links", "get", Some(&query.path))?;
    Ok(Json(lock(&state)?.get_link(&query.path)?))
}

async fn delete_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<StatusCode> {
    require(&state, &headers, "links", "delete", Some(&query.path))?;
    lock(&state)?.delete_link(&query.path)?;
    Ok(StatusCode::NO_CONTENT)
}

fn manifest_name(state: &AppState, path: &str) -> ApiResult<String> {
    Ok(lock(state)?.get_manifest(path)?.name)
}

fn require(
    state: &AppState,
    headers: &HeaderMap,
    resource: &str,
    verb: &str,
    path: Option<&str>,
) -> ApiResult<AuthContext> {
    let auth = authenticate(state, headers)?;
    if !kas_auth::allows(&auth.rules, resource, verb, path) {
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
    let auth = lock(state)?
        .authenticate(value)
        .map_err(|_| unauthorized())?;
    Ok(auth)
}

fn require_driver_ownership(
    auth: &AuthContext,
    driver_path: &str,
    generation: u64,
) -> ApiResult<()> {
    if let Some(auth_driver_path) = auth.driver_path.as_deref() {
        if auth_driver_path != driver_path || auth.driver_generation != Some(generation) {
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
