use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, Query, Request, State, WebSocketUpgrade,
    },
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
    driver_connections: Arc<Mutex<HashMap<Uuid, Uuid>>>,
}

pub fn app(store: Store) -> Router {
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        driver_connections: Arc::new(Mutex::new(HashMap::new())),
    };
    let protected = Router::new()
        .route("/manifests", get(list_manifests).post(create_manifest))
        .route("/manifests/{id}/driver", get(get_manifest_driver))
        .route("/resources", get(list_resources).post(create_resource))
        .route(
            "/resources/{id}",
            get(get_resource).patch(update_resource_spec),
        )
        .route(
            "/resources/{id}/status",
            axum::routing::put(update_resource_status),
        )
        .route("/drivers/{id}", get(get_driver).patch(update_driver))
        .route("/drivers/{id}/claim", post(claim_work))
        .route("/drivers/{id}/connect", get(connect_driver))
        .route("/drivers/{id}/credentials", post(issue_driver_credential))
        .route("/runs", post(enqueue_run))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/result", axum::routing::put(finish_run))
        .route("/links", get(list_links).post(create_link))
        .route("/links/{id}", get(get_link).delete(delete_link))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}/credentials", post(issue_user_credential))
        .route(
            "/service-accounts",
            get(list_service_accounts).post(create_service_account),
        )
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

struct ActiveWatch {
    cursor: u64,
    selectors: Vec<WatchSelector>,
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
) -> ApiResult<Json<Option<Driver>>> {
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

async fn update_resource_spec(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<UpdateResource>,
) -> ApiResult<Json<Resource>> {
    require(&state, &headers, "resources", "patch")?;
    Ok(Json(lock(&state)?.update_resource(id, input)?))
}

async fn update_resource_status(
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

#[derive(Debug, Deserialize)]
struct DriverConnectQuery {
    generation: u64,
}

async fn connect_driver(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<DriverConnectQuery>,
) -> ApiResult<Response> {
    let auth = require(&state, &headers, "drivers/connect", "create")?;
    require_driver_ownership(&auth, id, query.generation)?;
    let driver = lock(&state)?.get_driver(id)?;
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
        .insert(id, connection_id);
    Ok(ws
        .on_upgrade(move |socket| {
            serve_driver_socket(state, auth, id, query.generation, connection_id, socket)
        })
        .into_response())
}

async fn serve_driver_socket(
    state: AppState,
    auth: AuthContext,
    driver_id: Uuid,
    generation: u64,
    connection_id: Uuid,
    mut socket: WebSocket,
) {
    let control_delivery = Uuid::new_v4();
    let initial_driver =
        match lock(&state).and_then(|store| store.get_driver(driver_id).map_err(Into::into)) {
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
                if !is_current_connection(&state, driver_id, connection_id) {
                    break;
                }
                if push_watch_events(&state, &mut socket, &mut watches).await.is_err() {
                    break;
                }
                let driver = match lock(&state).and_then(|store| store.get_driver(driver_id).map_err(Into::into)) {
                    Ok(driver) if driver.generation == generation => driver,
                    _ => break,
                };
                match driver.state {
                    kas_core::DriverState::Ready if in_flight.is_none() => {
                        let delivery = match lock(&state).and_then(|mut store| store.claim_driver_delivery(driver_id, generation).map_err(Into::into)) {
                            Ok(delivery) => delivery,
                            Err(error) => {
                                eprintln!("Driver {driver_id} delivery claim failed: {}", error.1);
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
                    driver_id,
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
                        eprintln!("Driver {driver_id} WebSocket message failed: {}", error.1);
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        }
    }
    if let Ok(mut connections) = state.driver_connections.lock() {
        if connections.get(&driver_id) == Some(&connection_id) {
            connections.remove(&driver_id);
        }
    }
}

struct DriverMessageContext<'a> {
    state: &'a AppState,
    auth: &'a AuthContext,
    driver_id: Uuid,
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
        driver_id,
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
            require_driver_ownership(auth, driver_id, ready_generation)?;
            if ready_generation != generation {
                return Err(forbidden());
            }
            let driver = lock(state)?.get_driver(driver_id)?;
            if driver.state == kas_core::DriverState::Starting {
                lock(state)?.mark_driver_ready(
                    driver_id,
                    DriverReady {
                        generation,
                        process_id,
                        metadata,
                    },
                )?;
            } else if driver.state == kas_core::DriverState::Ready {
                lock(state)?.heartbeat_driver(driver_id, generation)?;
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
                lock(state)?.acknowledge_driver_delivery(delivery_id, driver_id, generation)?;
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
                driver_id,
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
            require_driver_ownership(auth, driver_id, stopped_generation)?;
            if stopped_generation != generation {
                return Err(forbidden());
            }
            lock(state)?.mark_driver_stopped(driver_id, generation)?;
        }
        ClientMessage::Pong => {
            lock(state)?.heartbeat_driver(driver_id, generation)?;
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn apply_driver_mutation(
    state: &AppState,
    auth: &AuthContext,
    driver_id: Uuid,
    generation: u64,
    in_flight: Option<Uuid>,
    request_id: Uuid,
    delivery_id: Uuid,
    driver_generation: u64,
    operations: Vec<Mutation>,
) -> ApiResult<Vec<Value>> {
    require_driver_ownership(auth, driver_id, driver_generation)?;
    if request_id != delivery_id || driver_generation != generation {
        return Err(forbidden());
    }
    ensure_in_flight(in_flight, delivery_id)?;
    let delivery = lock(state)?.get_driver_delivery(delivery_id)?;
    if delivery.driver_id != driver_id || delivery.generation != generation {
        return Err(forbidden());
    }
    match delivery.work {
        DriverWork::Reconcile { resource, revision } => {
            let [Mutation::UpdateResourceStatus {
                resource_id,
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
            if *resource_id != resource.id || *observed_revision != revision {
                return Err(forbidden());
            }
            if !kas_auth::allows(&auth.rules, "resources/status", "update") {
                return Err(forbidden());
            }
            let resource = lock(state)?.finish_reconciliation_delivery(
                delivery_id,
                driver_id,
                generation,
                *resource_id,
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
            let Mutation::CompleteRun { run_id, result } = completion else {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "A Run mutation must end with complete_run".into(),
                ));
            };
            if *run_id != run.id {
                return Err(forbidden());
            }
            authorize_mutations(state, auth, mutations)?;
            let run = lock(state)?.finish_run_delivery_with_mutations(
                delivery_id,
                driver_id,
                generation,
                *run_id,
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
    for mutation in mutations {
        let (resource, verb) = match mutation {
            Mutation::CreateResource { resource } => {
                let manifest = lock(state)?.get_manifest(resource.manifest_id)?;
                (format!("resources/{}", manifest.name), "create")
            }
            Mutation::UpdateResource { resource_id, .. } => {
                let resource = lock(state)?.get_resource(*resource_id)?;
                let manifest = lock(state)?.get_manifest(resource.manifest_id)?;
                (format!("resources/{}", manifest.name), "patch")
            }
            Mutation::CreateLink { .. } => ("links".into(), "create"),
            Mutation::UpdateResourceStatus { .. } | Mutation::CompleteRun { .. } => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "Lifecycle operations may only appear in their delivery-specific position"
                        .into(),
                ));
            }
        };
        if !kas_auth::allows(&auth.rules, &resource, verb) {
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
        let resource = match selector {
            WatchSelector::Resource {
                manifest_id: Some(manifest_id),
            } => format!("resources/{}", manifest_name(state, *manifest_id)?),
            WatchSelector::Resource { manifest_id: None } => "resources/*".into(),
            WatchSelector::Link { .. } => "links".into(),
            WatchSelector::Run { .. } => "runs".into(),
        };
        if !kas_auth::allows(&auth.rules, &resource, "watch") {
            return Err(forbidden());
        }
    }
    Ok(())
}

async fn push_watch_events(
    state: &AppState,
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
            if matches {
                let message = watch_event_message(watch_id, event);
                send_server_message(socket, &message).await?;
            }
        }
    }
    Ok(())
}

fn selector_matches(selector: &WatchSelector, event: &Event) -> bool {
    match selector {
        WatchSelector::Resource { manifest_id } => {
            event.object_kind == ObjectKind::Resource
                && manifest_id.is_none_or(|id| event.manifest_id == Some(id))
        }
        WatchSelector::Link {
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
            relation
                .as_ref()
                .is_none_or(|value| &link.relation == value)
                && source.as_ref().is_none_or(|value| &link.source == value)
                && target.as_ref().is_none_or(|value| &link.target == value)
        }
        WatchSelector::Run { resource_id } => {
            if event.object_kind != ObjectKind::Run {
                return false;
            }
            resource_id.is_none_or(|resource_id| {
                serde_json::from_value::<Run>(event.value.clone())
                    .is_ok_and(|run| run.resource_id == resource_id)
            })
        }
    }
}

fn watch_event_message(watch_id: Uuid, event: Event) -> ServerMessage {
    let object = WatchObject {
        kind: event.object_kind,
        id: event.object_id,
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

fn is_current_connection(state: &AppState, driver_id: Uuid, connection_id: Uuid) -> bool {
    state
        .driver_connections
        .lock()
        .ok()
        .and_then(|connections| connections.get(&driver_id).copied())
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

async fn list_service_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<ServiceAccount>>> {
    require(&state, &headers, "serviceaccounts", "list")?;
    Ok(Json(lock(&state)?.list_service_accounts()?))
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

async fn create_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateLink>,
) -> ApiResult<(StatusCode, Json<Link>)> {
    require(&state, &headers, "links", "create")?;
    Ok((StatusCode::CREATED, Json(lock(&state)?.create_link(input)?)))
}

#[derive(Debug, Deserialize)]
struct LinkQuery {
    source_kind: Option<ObjectKind>,
    source_id: Option<Uuid>,
    relation: Option<String>,
    target_kind: Option<ObjectKind>,
    target_id: Option<Uuid>,
}

fn optional_object_ref(
    kind: Option<ObjectKind>,
    id: Option<Uuid>,
    label: &str,
) -> ApiResult<Option<ObjectRef>> {
    match (kind, id) {
        (Some(kind), Some(id)) => Ok(Some(ObjectRef { kind, id })),
        (None, None) => Ok(None),
        _ => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("{label}_kind and {label}_id must be provided together"),
        )),
    }
}

async fn list_links(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LinkQuery>,
) -> ApiResult<Json<Vec<Link>>> {
    require(&state, &headers, "links", "list")?;
    let filter = LinkFilter {
        source: optional_object_ref(query.source_kind, query.source_id, "source")?,
        relation: query.relation,
        target: optional_object_ref(query.target_kind, query.target_id, "target")?,
    };
    Ok(Json(lock(&state)?.list_links(filter)?))
}

async fn get_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Link>> {
    require(&state, &headers, "links", "get")?;
    Ok(Json(lock(&state)?.get_link(id)?))
}

async fn delete_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require(&state, &headers, "links", "delete")?;
    lock(&state)?.delete_link(id)?;
    Ok(StatusCode::NO_CONTENT)
}

fn manifest_name(state: &AppState, id: Uuid) -> ApiResult<String> {
    Ok(lock(state)?.get_manifest(id)?.name)
}

fn require(
    state: &AppState,
    headers: &HeaderMap,
    resource: &str,
    verb: &str,
) -> ApiResult<AuthContext> {
    let auth = authenticate(state, headers)?;
    if !kas_auth::allows(&auth.rules, resource, verb) {
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
