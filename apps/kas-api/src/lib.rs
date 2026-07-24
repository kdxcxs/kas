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
use kas_auth::{
    AuthContext, CreateRole, CreateRoleBinding, CreateServiceAccount, CreateUser, IssuedCredential,
    Role, RoleBinding, Rule, ServiceAccount, User,
};
use kas_core::{
    CreateLink, CreateResource, CreateRun, Driver, DriverDesiredState, DriverReady, DriverWork,
    FinishRun, Link, LinkFilter, Manifest, Mutation, ObjectKind, ObjectRef, Resource,
    RestartPolicy, Run, UpdateResource, UpdateResourceStatus,
};
use kas_driver::{ClientMessage, MutationError, MutationStatus, ServerMessage};
use kas_store::{Store, StoreError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

mod package;
mod supervisor;

use supervisor::{DriverLaunch, Supervisor};

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
        .route(
            "/manifests",
            get(list_manifests)
                .post(create_manifest)
                .layer(DefaultBodyLimit::max(256 * 1024 * 1024)),
        )
        .route("/manifests/driver", get(get_manifest_driver))
        .route("/resources", get(list_resources).post(create_resource))
        .route(
            "/resources/by-path",
            get(get_resource)
                .patch(update_resource_spec)
                .delete(delete_resource),
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
        .route(
            "/credentials/by-path",
            axum::routing::delete(revoke_credential),
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

fn recover_drivers(state: &AppState) {
    let drivers = match lock(state).and_then(|store| store.list_drivers().map_err(Into::into)) {
        Ok(drivers) => drivers,
        Err(error) => {
            eprintln!("Driver recovery could not list Drivers: {}", error.1);
            return;
        }
    };
    for driver in drivers {
        if driver.desired_state != DriverDesiredState::Running {
            continue;
        }
        match driver_launch(state, &driver.path, None) {
            Ok(launch) => {
                if driver.state == kas_core::DriverState::Failed
                    && launch.definition.restart == RestartPolicy::Never
                {
                    continue;
                }
                if let Err(error) = state.supervisor.ensure_running(launch) {
                    eprintln!("Driver {} recovery failed: {error:#}", driver.path);
                }
            }
            Err(error) => eprintln!("Driver {} recovery failed: {}", driver.path, error.1),
        }
    }
}

fn driver_launch(
    state: &AppState,
    driver_path: &str,
    prepared_generation: Option<u64>,
) -> ApiResult<DriverLaunch> {
    let manifest = lock(state)?
        .list_manifests()?
        .into_iter()
        .find(|manifest| {
            manifest
                .driver
                .as_ref()
                .is_some_and(|driver| driver.path == driver_path)
        })
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                format!("Manifest for Driver {driver_path} not found"),
            )
        })?;
    let definition = manifest.driver.clone().expect("Driver was matched");
    let hex = manifest
        .package_digest
        .strip_prefix("sha256:")
        .ok_or_else(|| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Manifest package digest is invalid".into(),
            )
        })?;
    let package_root = state.data_dir.join("packages").join("sha256").join(hex);
    if !package_root.is_dir() || !package_root.join(&definition.entrypoint).is_file() {
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Driver {driver_path} package or entrypoint is missing"),
        ));
    }
    Ok(DriverLaunch {
        manifest_path: manifest.path,
        package_root,
        definition,
        prepared_generation,
    })
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true}))
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
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Manifest>)> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim() == "application/vnd.kas.manifest+tar")
    {
        return Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "POST /manifests requires application/vnd.kas.manifest+tar".into(),
        ));
    }
    let preview_body = body.clone();
    let preview = tokio::task::spawn_blocking(move || package::inspect(&preview_body))
        .await
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    let auth = require(&state, &headers, "manifests", "create", Some(&preview.path))?;
    authorize_manifest_install(&state, &auth, &preview)?;
    match lock(&state)?.get_manifest(&preview.path) {
        Ok(existing) if existing.package_digest == preview.package_digest => {
            return Ok((StatusCode::CREATED, Json(existing)));
        }
        Ok(_) | Err(StoreError::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    let data_dir = state.data_dir.clone();
    let installed = tokio::task::spawn_blocking(move || package::install(&data_dir, &body))
        .await
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    let manifest = lock(&state)?.install_manifest(installed.manifest, installed.size_bytes)?;
    if let Some(definition) = manifest.driver.clone() {
        let driver = lock(&state)?.start_driver(&definition.path)?;
        state
            .supervisor
            .ensure_running(DriverLaunch {
                manifest_path: manifest.path.clone(),
                package_root: installed.root,
                definition,
                prepared_generation: Some(driver.generation),
            })
            .map_err(internal_error)?;
    }
    Ok((StatusCode::CREATED, Json(manifest)))
}

fn authorize_manifest_install(
    state: &AppState,
    auth: &AuthContext,
    manifest: &kas_core::CreateManifest,
) -> ApiResult<()> {
    for account in &manifest.rbac.service_accounts {
        if !kas_auth::allows(
            &auth.rules,
            "serviceaccounts",
            "create",
            Some(&account.path),
        ) {
            return Err(forbidden());
        }
    }
    for role in &manifest.rbac.roles {
        if !kas_auth::allows(&auth.rules, "roles", "create", Some(&role.path)) {
            return Err(forbidden());
        }
        let rules = role
            .rules
            .iter()
            .map(|rule| Rule {
                resources: rule.resources.clone(),
                verbs: rule.verbs.clone(),
                paths: rule.paths.clone(),
            })
            .collect::<Vec<_>>();
        if !kas_auth::allows(&auth.rules, "roles", "escalate", Some(&role.path))
            && !kas_auth::rules_are_subset(&rules, &auth.rules)
        {
            return Err(forbidden());
        }
    }
    for binding in &manifest.rbac.role_bindings {
        if !kas_auth::allows(&auth.rules, "rolebindings", "create", Some(&binding.path)) {
            return Err(forbidden());
        }
        let role_rules = if let Some(role) = manifest
            .rbac
            .roles
            .iter()
            .find(|role| role.path == binding.role_path)
        {
            role.rules
                .iter()
                .map(|rule| Rule {
                    resources: rule.resources.clone(),
                    verbs: rule.verbs.clone(),
                    paths: rule.paths.clone(),
                })
                .collect()
        } else {
            lock(state)?.get_role(&binding.role_path)?.rules
        };
        if !kas_auth::allows(&auth.rules, "roles", "bind", Some(&binding.role_path))
            && !kas_auth::rules_are_subset(&role_rules, &auth.rules)
        {
            return Err(forbidden());
        }
    }
    Ok(())
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
    let manifest = lock(&state)?.get_manifest(&input.manifest)?;
    let auth = require(
        &state,
        &headers,
        &format!("resources/{}", manifest.name),
        "create",
        Some(&input.path),
    )?;
    let pending_resources = HashMap::from([(input.path.clone(), manifest.name)]);
    for link in &input.links {
        authorize_relation_use(&auth, &link.relation_path)?;
        authorize_link_endpoint(&state, &auth, &link.source, Some(&pending_resources))?;
        authorize_link_endpoint(&state, &auth, &link.target, Some(&pending_resources))?;
    }
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
            manifest_for_resource_in_store(&store, &resource.path).is_ok_and(|manifest| {
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
    Query(query): Query<ResourceQuery>,
) -> ApiResult<Json<Value>> {
    let resource = lock(&state)?.get_resource(&query.path)?;
    let manifest = manifest_for_resource(&state, &resource.path)?;
    let auth = require(
        &state,
        &headers,
        &format!("resources/{}", manifest.name),
        "get",
        Some(&resource.path),
    )?;
    let mut value = serde_json::to_value(&resource).map_err(internal_error)?;
    if query.include.as_deref() == Some("relations") {
        let resource_ref = ObjectRef {
            kind: ObjectKind::Resource,
            path: resource.path.clone(),
        };
        let store = lock(&state)?;
        let links = store
            .links_for_object(&resource_ref)?
            .into_iter()
            .filter(|link| {
                kas_auth::allows(&auth.rules, "links", "list", Some(&link.path))
                    || kas_auth::allows(&auth.rules, "links", "get", Some(&link.path))
            })
            .collect::<Vec<_>>();
        let mut related_refs = Vec::new();
        for link in &links {
            let related = if link.source.as_ref() == Some(&resource_ref) {
                link.target.as_ref()
            } else {
                link.source.as_ref()
            };
            if let Some(related) = related {
                if !related_refs.contains(related) {
                    related_refs.push(related.clone());
                }
            }
        }
        let related = related_refs
            .into_iter()
            .filter(|object| object_is_readable(&store, &auth, object))
            .map(|object| {
                let object_value = store.object_value(&object)?;
                Ok(json!({
                    "kind": object.kind,
                    "path": object.path,
                    "value": object_value
                }))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let object = value
            .as_object_mut()
            .expect("Resource serializes as an object");
        object.insert(
            "links".into(),
            serde_json::to_value(links).map_err(internal_error)?,
        );
        object.insert("related".into(), Value::Array(related));
    } else if query.include.is_some() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "include must be relations when provided".into(),
        ));
    }
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
struct ResourceQuery {
    path: String,
    include: Option<String>,
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
    let resource = lock(&state)?.get_resource(&query.path)?;
    let manifest = manifest_for_resource(&state, &resource.path)?;
    require(
        &state,
        &headers,
        &format!("resources/{}", manifest.name),
        "delete",
        Some(&resource.path),
    )?;
    Ok(Json(
        lock(&state)?.delete_resource(&query.path, query.expected_revision)?,
    ))
}

async fn update_resource_spec(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
    Json(input): Json<UpdateResource>,
) -> ApiResult<Json<Resource>> {
    let resource = lock(&state)?.get_resource(&query.path)?;
    let manifest = manifest_for_resource(&state, &resource.path)?;
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
    let auth = authenticate(&state, &headers)?;
    if auth.driver_path.is_some() {
        require_bound_driver(&auth, &input.driver_path, input.driver_generation)?;
    } else if !kas_auth::allows(&auth.rules, "resources/status", "update", Some(&query.path)) {
        return Err(forbidden());
    }
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
    let auth = authenticate(&state, &headers)?;
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
        require_bound_driver(&auth, &query.path, generation)?;
    } else if !kas_auth::allows(&auth.rules, "drivers", "patch", Some(&query.path)) {
        return Err(forbidden());
    }
    let driver = match input {
        DriverUpdate::Starting => {
            let driver = lock(&state)?.start_driver(&query.path)?;
            let launch = driver_launch(&state, &query.path, Some(driver.generation))?;
            state
                .supervisor
                .ensure_running(launch)
                .map_err(internal_error)?;
            driver
        }
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
        DriverUpdate::Stopping => {
            let driver = lock(&state)?.stop_driver(&query.path)?;
            state
                .supervisor
                .stop(query.path.clone())
                .map_err(internal_error)?;
            driver
        }
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
    let auth = require(&state, &headers, "runs", "create", Some(&input.path))?;
    if !kas_auth::allows_action_invoke(&auth.rules, &input.action) {
        return Err(forbidden());
    }
    for link in &input.links {
        authorize_relation_use(&auth, &link.relation_path)?;
        authorize_link_endpoint(&state, &auth, &link.source, None)?;
        authorize_link_endpoint(&state, &auth, &link.target, None)?;
    }
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
    let auth = authenticate(&state, &headers)?;
    require_bound_driver(&auth, &query.path, input.generation)?;
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
    let auth = authenticate(&state, &headers)?;
    require_bound_driver(&auth, &query.path, query.generation)?;
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
    let mut in_flight: Option<Uuid> = None;
    let mut stop_delivery: Option<Uuid> = None;
    let mut stop_acked = false;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if !is_current_connection(&state, &driver_path, connection_id) {
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
                                DriverWork::Reconcile { object } => ServerMessage::Reconcile {
                                    delivery_id: delivery.id,
                                    object,
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
                match handle_driver_message(
                    context,
                    &mut in_flight,
                    &mut stop_acked,
                    message,
                ) {
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
        DriverWork::Reconcile { .. } => {
            let ordinary = operations
                .iter()
                .filter(|operation| !matches!(operation, Mutation::UpdateResourceStatus { .. }))
                .cloned()
                .collect::<Vec<_>>();
            authorize_mutations(state, auth, &ordinary)?;
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
                let manifest = lock(state)?.get_manifest(&resource.manifest)?;
                pending_resources.insert(resource.path.clone(), manifest.name.clone());
                for link in &resource.links {
                    authorize_relation_use(auth, &link.relation_path)?;
                    authorize_link_endpoint(state, auth, &link.source, Some(&pending_resources))?;
                    authorize_link_endpoint(state, auth, &link.target, Some(&pending_resources))?;
                }
                (
                    format!("resources/{}", manifest.name),
                    "create",
                    resource.path.as_str(),
                )
            }
            Mutation::UpdateResource { resource_path, .. } => {
                let manifest = manifest_for_resource(state, resource_path)?;
                (
                    format!("resources/{}", manifest.name),
                    "patch",
                    resource_path.as_str(),
                )
            }
            Mutation::DeleteResource { resource_path, .. } => {
                let manifest = manifest_for_resource(state, resource_path)?;
                (
                    format!("resources/{}", manifest.name),
                    "delete",
                    resource_path.as_str(),
                )
            }
            Mutation::CreateLink { link } => {
                authorize_relation_use(auth, &link.relation_path)?;
                authorize_link_endpoint(state, auth, &link.source, Some(&pending_resources))?;
                authorize_link_endpoint(state, auth, &link.target, Some(&pending_resources))?;
                ("links".into(), "create", link.path.as_str())
            }
            Mutation::UpdateLink {
                link_path,
                source,
                target,
                ..
            } => {
                authorize_link_endpoint(state, auth, source, Some(&pending_resources))?;
                authorize_link_endpoint(state, auth, target, Some(&pending_resources))?;
                ("links".into(), "patch", link_path.as_str())
            }
            Mutation::DeleteLink { link_path } => ("links".into(), "delete", link_path.as_str()),
            Mutation::CreateServiceAccount { path, .. } => {
                ("serviceaccounts".into(), "create", path.as_str())
            }
            Mutation::DeleteServiceAccount { path } => {
                ("serviceaccounts".into(), "delete", path.as_str())
            }
            Mutation::CreateRoleBinding {
                path, role_path, ..
            } => {
                if !kas_auth::allows(&auth.rules, "roles", "bind", Some(role_path)) {
                    return Err(forbidden());
                }
                ("rolebindings".into(), "create", path.as_str())
            }
            Mutation::DeleteRoleBinding { path } => {
                ("rolebindings".into(), "delete", path.as_str())
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
    let auth = authenticate(&state, &headers)?;
    if let Some(driver_path) = auth.driver_path.as_deref() {
        require_bound_driver(&auth, driver_path, input.driver_generation)?;
    } else if !kas_auth::allows(&auth.rules, "runs/result", "update", Some(&query.path)) {
        return Err(forbidden());
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

async fn revoke_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectPathQuery>,
) -> ApiResult<StatusCode> {
    require(&state, &headers, "credentials", "delete", Some(&query.path))?;
    lock(&state)?.revoke_credential(&query.path)?;
    Ok(StatusCode::NO_CONTENT)
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
    authorize_relation_use(&auth, &input.relation_path)?;
    authorize_link_endpoint(&state, &auth, &input.source, None)?;
    authorize_link_endpoint(&state, &auth, &input.target, None)?;
    Ok((StatusCode::CREATED, Json(lock(&state)?.create_link(input)?)))
}

fn authorize_relation_use(auth: &AuthContext, relation_path: &str) -> ApiResult<()> {
    if !kas_auth::allows_relation_use(&auth.rules, relation_path) {
        return Err(forbidden());
    }
    Ok(())
}

fn object_is_readable(store: &Store, auth: &AuthContext, object: &ObjectRef) -> bool {
    let resource = match object.kind {
        ObjectKind::Manifest => "manifests".into(),
        ObjectKind::Resource => {
            let Ok(manifest) = manifest_for_resource_in_store(store, &object.path) else {
                return false;
            };
            format!("resources/{}", manifest.name)
        }
        ObjectKind::Action => "actions".into(),
        ObjectKind::Relation => "relations".into(),
        ObjectKind::Driver => "drivers".into(),
        ObjectKind::Run => "runs".into(),
        ObjectKind::Link => "links".into(),
        ObjectKind::User => "users".into(),
        ObjectKind::ServiceAccount => "serviceaccounts".into(),
        ObjectKind::Role => "roles".into(),
        ObjectKind::RoleBinding => "rolebindings".into(),
        ObjectKind::Credential => "credentials".into(),
    };
    kas_auth::allows(&auth.rules, &resource, "get", Some(&object.path))
}

fn authorize_link_endpoint(
    state: &AppState,
    auth: &AuthContext,
    object: &Option<ObjectRef>,
    pending_resources: Option<&HashMap<String, String>>,
) -> ApiResult<()> {
    let Some(object) = object else {
        return Ok(());
    };
    let resource = match object.kind {
        ObjectKind::Manifest => "manifests".into(),
        ObjectKind::Resource => {
            let manifest_name = if let Some(name) =
                pending_resources.and_then(|resources| resources.get(&object.path))
            {
                name.clone()
            } else {
                manifest_for_resource(state, &object.path)?.name
            };
            format!("resources/{manifest_name}")
        }
        ObjectKind::Action => "actions".into(),
        ObjectKind::Relation => "relations".into(),
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
    relation_path: Option<String>,
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
        relation_path: query.relation_path,
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

fn manifest_for_resource(state: &AppState, resource_path: &str) -> ApiResult<Manifest> {
    let store = lock(state)?;
    manifest_for_resource_in_store(&store, resource_path)
}

fn manifest_for_resource_in_store(store: &Store, resource_path: &str) -> ApiResult<Manifest> {
    let manifest_path = store.manifest_path_for_resource(resource_path)?;
    Ok(store.get_manifest(&manifest_path)?)
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
