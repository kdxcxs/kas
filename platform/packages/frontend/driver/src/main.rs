use std::{
    collections::HashMap,
    env,
    io::Cursor,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use axum::{
    body::Body,
    extract::{OriginalUri, Path as AxumPath, Request, State},
    http::{
        header::{
            ACCESS_CONTROL_ALLOW_ORIGIN, AUTHORIZATION, CONTENT_TYPE, COOKIE, HOST, SET_COOKIE,
        },
        HeaderMap, HeaderValue, Method, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Json, Router,
};
use kas_core::{DriverExecution, Mutation, Resource, ResourceStatus};
use kas_driver::{Driver, DriverError, DriverRuntime};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    fs,
    net::TcpListener,
    sync::RwLock,
};
use uuid::Uuid;
use zip::ZipArchive;

const FRONTEND_PLUGIN_MANIFEST: &str = "/manifests/frontend-plugin";
const PROXY_MANIFEST: &str = "/manifests/proxy";
const FILE_MANIFEST: &str = "/manifests/file";
const LINK_MANIFEST: &str = "/builtin/link";
const BUNDLE_RELATION: &str = "/manifests/frontend-plugin/relations/bundle";
const MAX_PLUGIN_FILES: usize = 4096;
const MAX_PLUGIN_UNCOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone)]
struct GatewayState {
    api: String,
    driver_token: String,
    web_root: PathBuf,
    client: Client,
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    plugins: Arc<RwLock<HashMap<String, PluginMount>>>,
    proxies: Arc<RwLock<HashMap<String, ProxyMount>>>,
}

#[derive(Clone)]
struct Session {
    token: String,
    subject: Value,
}

#[derive(Clone)]
struct PluginMount {
    resource_path: String,
    root: PathBuf,
    entrypoint: String,
}

#[derive(Clone)]
struct ProxyMount {
    resource_path: String,
    prefix: String,
    target: String,
    strip_prefix: bool,
    authorization: ProxyAuthorization,
}

#[derive(Debug, Clone, Deserialize)]
struct FrontendPluginSpec {
    slug: String,
    entrypoint: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ProxySpec {
    prefix: String,
    upstream: String,
    strip_prefix: bool,
    authorization: ProxyAuthorization,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProxyAuthorization {
    Session,
    Passthrough,
    None,
}

#[derive(Debug, Deserialize)]
struct CreateSession {
    token: String,
}

#[derive(Debug, Serialize)]
struct SessionView {
    subject: Value,
}

#[derive(Clone)]
struct FrontendDriver {
    state: GatewayState,
    plugin_root: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api = normalized_url(env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into()));
    let address: SocketAddr = env::var("KAS_FRONTEND_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:5173".into())
        .parse()?;
    let package_root = PathBuf::from(env::var_os("KAS_PACKAGE_ROOT").ok_or_else(|| {
        anyhow::anyhow!("KAS_PACKAGE_ROOT is required")
    })?);
    let web_root = package_root.join("driver/web");
    if !web_root.join("index.html").is_file() {
        anyhow::bail!("Frontend web root is missing {}", web_root.display());
    }
    let data_dir = PathBuf::from(
        env::var_os("KAS_DATA_DIR").ok_or_else(|| anyhow::anyhow!("KAS_DATA_DIR is required"))?,
    );
    let plugin_root = data_dir.join("frontend-driver").join("plugins");
    fs::create_dir_all(&plugin_root).await?;

    let driver_path = env::var("KAS_DRIVER_PATH")?;
    let generation = env::var("KAS_DRIVER_GENERATION")?.parse()?;
    let driver_token = env::var("KAS_DRIVER_TOKEN")?;
    let state = GatewayState {
        api: api.clone(),
        driver_token: driver_token.clone(),
        web_root: web_root.clone(),
        client: Client::new(),
        sessions: Default::default(),
        plugins: Default::default(),
        proxies: Default::default(),
    };
    let driver = FrontendDriver {
        state: state.clone(),
        plugin_root,
    };
    let runtime = DriverRuntime::new(api, driver_path, generation, driver_token, driver);
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .route(
            "/gateway/session",
            post(create_session).get(get_session).delete(delete_session),
        )
        .route("/plugins/{slug}", get(plugin_entrypoint))
        .route("/plugins/{slug}/{*asset}", get(plugin_asset).head(plugin_asset))
        .route("/api", any(proxy_core))
        .route("/api/{*rest}", any(proxy_core))
        .fallback(dynamic_request)
        .with_state(state);
    let listener = TcpListener::bind(address).await?;
    tokio::select! {
        result = axum::serve(listener, app) => result.map_err(Into::into),
        result = runtime.run() => result,
    }
}

async fn create_session(
    State(state): State<GatewayState>,
    Json(input): Json<CreateSession>,
) -> Result<Response, GatewayError> {
    let token = input.token.trim();
    if token.is_empty() {
        return Err(GatewayError::new(StatusCode::BAD_REQUEST, "token is required"));
    }
    let response = state
        .client
        .get(format!("{}/auth", state.api))
        .bearer_auth(token)
        .send()
        .await
        .map_err(internal)?;
    if !response.status().is_success() {
        return Err(GatewayError::new(
            StatusCode::UNAUTHORIZED,
            "credential was rejected",
        ));
    }
    let subject: Value = response.json().await.map_err(internal)?;
    let id = Uuid::new_v4().simple().to_string();
    state.sessions.write().await.insert(
        id.clone(),
        Session {
            token: token.to_owned(),
            subject: subject.clone(),
        },
    );
    let mut response = Json(SessionView { subject }).into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&format!(
            "kas_session={id}; HttpOnly; SameSite=Strict; Path=/"
        ))
        .map_err(internal)?,
    );
    Ok(response)
}

async fn get_session(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Result<Json<SessionView>, GatewayError> {
    let session = current_session(&state, &headers)
        .await
        .ok_or_else(|| GatewayError::new(StatusCode::UNAUTHORIZED, "session is required"))?;
    Ok(Json(SessionView {
        subject: session.subject,
    }))
}

async fn delete_session(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    if let Some(id) = session_id(&headers) {
        state.sessions.write().await.remove(&id);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_static(
            "kas_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        ),
    );
    Ok(response)
}

async fn proxy_core(
    State(state): State<GatewayState>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Result<Response, GatewayError> {
    proxy_to(
        &state,
        &uri,
        request,
        "/api",
        &state.api,
        true,
        ProxyAuthorization::Session,
    )
    .await
}

async fn dynamic_request(
    State(state): State<GatewayState>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Result<Response, GatewayError> {
    let rule = {
        let proxies = state.proxies.read().await;
        proxies
            .values()
            .filter(|rule| prefix_matches(uri.path(), &rule.prefix))
            .max_by_key(|rule| rule.prefix.len())
            .cloned()
    };
    if let Some(rule) = rule {
        return proxy_to(
            &state,
            &uri,
            request,
            &rule.prefix,
            &rule.target,
            rule.strip_prefix,
            rule.authorization,
        )
        .await;
    }
    serve_host(&state, uri.path(), request.method()).await
}

async fn proxy_to(
    state: &GatewayState,
    uri: &axum::http::Uri,
    request: Request,
    prefix: &str,
    upstream: &str,
    strip_prefix: bool,
    authorization: ProxyAuthorization,
) -> Result<Response, GatewayError> {
    let suffix = if strip_prefix {
        uri.path().strip_prefix(prefix).unwrap_or("")
    } else {
        uri.path()
    };
    let target = format!(
        "{}{}{}",
        upstream,
        if suffix.is_empty() { "/" } else { suffix },
        uri.query().map(|query| format!("?{query}")).unwrap_or_default()
    );
    let (parts, body) = request.into_parts();
    let original_authorization = parts.headers.get(AUTHORIZATION).cloned();
    let mut builder = state.client.request(parts.method, target);
    for (name, value) in &parts.headers {
        if name != HOST && name != COOKIE && name != AUTHORIZATION {
            builder = builder.header(name, value);
        }
    }
    match authorization {
        ProxyAuthorization::Session => {
            if let Some(token) = request_token(state, &parts.headers).await {
                builder = builder.bearer_auth(token);
            }
        }
        ProxyAuthorization::Passthrough => {
            if let Some(value) = original_authorization {
                builder = builder.header(AUTHORIZATION, value);
            }
        }
        ProxyAuthorization::None => {}
    }
    let upstream = builder
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
        .map_err(internal)?;
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut response = Response::builder().status(status);
    for (name, value) in &headers {
        if name.as_str() != "transfer-encoding" && name.as_str() != "connection" {
            response = response.header(name, value);
        }
    }
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(internal)
}

async fn serve_host(
    state: &GatewayState,
    request_path: &str,
    method: &Method,
) -> Result<Response, GatewayError> {
    if method != Method::GET && method != Method::HEAD {
        return Err(GatewayError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "host assets only support GET and HEAD",
        ));
    }
    let relative = request_path.trim_start_matches('/');
    let candidate = if relative.is_empty() {
        state.web_root.join("index.html")
    } else {
        safe_relative_path(relative)
            .map(|path| state.web_root.join(path))
            .unwrap_or_else(|_| state.web_root.join("index.html"))
    };
    let path = if candidate.is_file() {
        candidate
    } else {
        state.web_root.join("index.html")
    };
    let bytes = if method == Method::HEAD {
        Vec::new()
    } else {
        fs::read(&path).await.map_err(internal)?
    };
    let media_type = mime_guess::from_path(&path).first_or_octet_stream();
    Ok((
        [(CONTENT_TYPE, HeaderValue::from_str(media_type.as_ref()).map_err(internal)?)],
        bytes,
    )
        .into_response())
}

fn prefix_matches(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

async fn plugin_entrypoint(
    State(state): State<GatewayState>,
    AxumPath(slug): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let mount = plugin_mount(&state, &slug, &headers).await?;
    serve_plugin_file(&mount, &mount.entrypoint).await
}

async fn plugin_asset(
    State(state): State<GatewayState>,
    AxumPath((slug, asset)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let candidate = plugin_mount_unchecked(&state, &slug).await?;
    let mount = if asset == candidate.entrypoint {
        plugin_mount(&state, &slug, &headers).await?
    } else {
        candidate
    };
    serve_plugin_file(&mount, &asset).await
}

async fn plugin_mount_unchecked(
    state: &GatewayState,
    slug: &str,
) -> Result<PluginMount, GatewayError> {
    state
        .plugins
        .read()
        .await
        .get(slug)
        .cloned()
        .ok_or_else(|| GatewayError::new(StatusCode::NOT_FOUND, "plugin was not found"))
}

async fn plugin_mount(
    state: &GatewayState,
    slug: &str,
    headers: &HeaderMap,
) -> Result<PluginMount, GatewayError> {
    let mount = plugin_mount_unchecked(state, slug).await?;
    let token = request_token(state, headers)
        .await
        .ok_or_else(|| GatewayError::new(StatusCode::UNAUTHORIZED, "session is required"))?;
    let response = state
        .client
        .post(format!("{}/auth/check", state.api))
        .bearer_auth(token)
        .json(&json!({
            "manifest": FRONTEND_PLUGIN_MANIFEST,
            "verb": "get",
            "path": mount.resource_path
        }))
        .send()
        .await
        .map_err(internal)?;
    let status = response.status();
    let decision: Value = response.json().await.map_err(internal)?;
    if !status.is_success()
        || !decision.get("allowed").and_then(Value::as_bool).unwrap_or(false)
    {
        return Err(GatewayError::new(StatusCode::FORBIDDEN, "plugin access denied"));
    }
    Ok(mount)
}

async fn serve_plugin_file(mount: &PluginMount, asset: &str) -> Result<Response, GatewayError> {
    let relative = safe_relative_path(asset)?;
    let path = mount.root.join(relative);
    if !path.is_file() {
        return Err(GatewayError::new(StatusCode::NOT_FOUND, "plugin asset was not found"));
    }
    let bytes = fs::read(&path).await.map_err(internal)?;
    let media_type = mime_guess::from_path(&path).first_or_octet_stream();
    Ok((
        [
            (
                CONTENT_TYPE,
                HeaderValue::from_str(media_type.as_ref()).map_err(internal)?,
            ),
            (ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*")),
        ],
        bytes,
    )
        .into_response())
}

async fn current_session(state: &GatewayState, headers: &HeaderMap) -> Option<Session> {
    let id = session_id(headers)?;
    state.sessions.read().await.get(&id).cloned()
}

async fn request_token(state: &GatewayState, headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(AUTHORIZATION).and_then(|value| value.to_str().ok()) {
        if let Some(token) = value.strip_prefix("Bearer ") {
            return Some(token.to_owned());
        }
    }
    current_session(state, headers).await.map(|session| session.token)
}

fn session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix("kas_session=").map(str::to_owned))
}

#[async_trait]
impl Driver for FrontendDriver {
    fn name(&self) -> &str {
        "frontend-gateway"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        match resource.manifest.as_str() {
            FRONTEND_PLUGIN_MANIFEST => self.reconcile_plugin(resource).await,
            PROXY_MANIFEST => self.reconcile_proxy(resource).await,
            LINK_MANIFEST if relation_of(resource) == Some(BUNDLE_RELATION) => {
                let Some(source) = source_of(resource) else {
                    return Ok(Vec::new());
                };
                let plugin = self.get_resource(source).await?;
                self.reconcile_plugin(&plugin).await
            }
            FILE_MANIFEST => {
                let resources = self.list_resources().await?;
                let mut mutations = Vec::new();
                for link in resources.iter().filter(|candidate| {
                    candidate.manifest == LINK_MANIFEST
                        && relation_of(candidate) == Some(BUNDLE_RELATION)
                        && target_of(candidate) == Some(resource.path.as_str())
                }) {
                    if let Some(source) = source_of(link) {
                        let plugin = self.get_resource(source).await?;
                        mutations.extend(self.reconcile_plugin(&plugin).await?);
                    }
                }
                Ok(mutations)
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn execute(
        &self,
        _resource: &Resource,
        action: &Resource,
        _run: &Resource,
    ) -> Result<DriverExecution, DriverError> {
        Err(DriverError::UnsupportedAction(action.path.clone()))
    }
}

impl FrontendDriver {
    async fn reconcile_proxy(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let spec: ProxySpec = serde_json::from_value(resource.spec.clone())
            .map_err(|error| execution(format!("invalid Proxy spec: {error}")))?;
        if resource.metadata.state == kas_core::STATE_DELETED {
            self.state.proxies.write().await.remove(&resource.path);
            return Ok(vec![status_mutation(resource)]);
        }
        validate_proxy_prefix(&spec.prefix)?;
        let upstream = reqwest::Url::parse(&spec.upstream)
            .map_err(|error| execution(format!("invalid Proxy upstream: {error}")))?;
        if upstream.scheme() != "http" && upstream.scheme() != "https" {
            return Err(execution("Proxy upstream must use http or https"));
        }
        let mut proxies = self.state.proxies.write().await;
        if let Some(conflict) = proxies.values().find(|candidate| {
            candidate.prefix == spec.prefix && candidate.resource_path != resource.path
        }) {
            return Err(execution(format!(
                "Proxy prefix {:?} is already owned by {}",
                spec.prefix, conflict.resource_path
            )));
        }
        proxies.insert(
            resource.path.clone(),
            ProxyMount {
                resource_path: resource.path.clone(),
                prefix: spec.prefix,
                target: normalized_url(spec.upstream),
                strip_prefix: spec.strip_prefix,
                authorization: spec.authorization,
            },
        );
        Ok(vec![status_mutation(resource)])
    }

    async fn reconcile_plugin(&self, plugin: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let spec: FrontendPluginSpec = serde_json::from_value(plugin.spec.clone())
            .map_err(|error| execution(format!("invalid FrontendPlugin spec: {error}")))?;
        if plugin.metadata.state == kas_core::STATE_DELETED {
            if let Some(mount) = self.state.plugins.write().await.remove(&spec.slug) {
                let _ = fs::remove_dir_all(mount.root).await;
            }
            return Ok(vec![status_mutation(plugin)]);
        }
        if let Some(existing) = self.state.plugins.read().await.get(&spec.slug) {
            if existing.resource_path != plugin.path {
                return Err(execution(format!(
                    "FrontendPlugin slug {:?} is already owned by {}",
                    spec.slug, existing.resource_path
                )));
            }
        }
        let resources = self.list_resources().await?;
        let link = resources
            .iter()
            .find(|candidate| {
                candidate.manifest == LINK_MANIFEST
                    && relation_of(candidate) == Some(BUNDLE_RELATION)
                    && source_of(candidate) == Some(plugin.path.as_str())
                    && candidate.metadata.state != kas_core::STATE_DELETED
            })
            .ok_or_else(|| execution(format!("FrontendPlugin {} has no bundle Link", plugin.path)))?;
        let file_path = target_of(link)
            .ok_or_else(|| execution("FrontendPlugin bundle Link has no target"))?;
        let file = resources
            .iter()
            .find(|candidate| candidate.path == file_path && candidate.manifest == FILE_MANIFEST)
            .ok_or_else(|| execution(format!("FrontendPlugin bundle File {file_path} was not found")))?;
        let digest = file
            .spec
            .get("digest")
            .and_then(Value::as_str)
            .ok_or_else(|| execution("FrontendPlugin bundle File has no digest"))?;
        let version_root = self
            .plugin_root
            .join(&spec.slug)
            .join(digest.strip_prefix("sha256:").unwrap_or(digest));
        if !version_root.join(&spec.entrypoint).is_file() {
            let bytes = self.download_file(file_path).await?;
            extract_plugin(bytes, version_root.clone(), spec.entrypoint.clone()).await?;
        }
        self.state.plugins.write().await.insert(
            spec.slug,
            PluginMount {
                resource_path: plugin.path.clone(),
                root: version_root,
                entrypoint: spec.entrypoint,
            },
        );
        Ok(vec![status_mutation(plugin)])
    }

    async fn list_resources(&self) -> Result<Vec<Resource>, DriverError> {
        let response = self
            .state
            .client
            .get(format!("{}/resources", self.state.api))
            .bearer_auth(&self.state.driver_token)
            .send()
            .await
            .map_err(|error| execution(error.to_string()))?;
        if !response.status().is_success() {
            return Err(execution(format!(
                "could not list plugin dependencies: {}",
                response.status()
            )));
        }
        response
            .json()
            .await
            .map_err(|error| execution(error.to_string()))
    }

    async fn get_resource(&self, path: &str) -> Result<Resource, DriverError> {
        let response = self
            .state
            .client
            .get(format!("{}/resources/by-path", self.state.api))
            .bearer_auth(&self.state.driver_token)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(|error| execution(error.to_string()))?;
        if !response.status().is_success() {
            return Err(execution(format!("could not get Resource {path}")));
        }
        response
            .json()
            .await
            .map_err(|error| execution(error.to_string()))
    }

    async fn download_file(&self, path: &str) -> Result<Vec<u8>, DriverError> {
        let file_api = {
            let proxies = self.state.proxies.read().await;
            proxies
                .values()
                .find(|proxy| proxy.prefix == "/files-api")
                .map(|proxy| proxy.target.clone())
                .ok_or_else(|| execution("the /files-api Proxy is not available"))?
        };
        let response = self
            .state
            .client
            .get(format!("{file_api}/files/content"))
            .bearer_auth(&self.state.driver_token)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(|error| execution(error.to_string()))?;
        if !response.status().is_success() {
            return Err(execution(format!(
                "could not download plugin bundle {path}: {}",
                response.status()
            )));
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| execution(error.to_string()))
    }
}

async fn extract_plugin(
    bytes: Vec<u8>,
    destination: PathBuf,
    entrypoint: String,
) -> Result<(), DriverError> {
    tokio::task::spawn_blocking(move || {
        if destination.exists() {
            std::fs::remove_dir_all(&destination).map_err(|error| execution(error.to_string()))?;
        }
        let temporary = destination.with_extension(format!("tmp-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&temporary).map_err(|error| execution(error.to_string()))?;
        let result = (|| {
            let mut archive =
                ZipArchive::new(Cursor::new(bytes)).map_err(|error| execution(error.to_string()))?;
            if archive.len() == 0 || archive.len() > MAX_PLUGIN_FILES {
                return Err(execution("FrontendPlugin ZIP contains an invalid number of files"));
            }
            let mut total = 0_u64;
            for index in 0..archive.len() {
                let mut file = archive
                    .by_index(index)
                    .map_err(|error| execution(error.to_string()))?;
                if file.is_symlink() {
                    return Err(execution("FrontendPlugin ZIP may not contain symbolic links"));
                }
                total = total
                    .checked_add(file.size())
                    .ok_or_else(|| execution("FrontendPlugin ZIP size overflow"))?;
                if total > MAX_PLUGIN_UNCOMPRESSED_BYTES {
                    return Err(execution("FrontendPlugin ZIP is too large after extraction"));
                }
                let enclosed = file
                    .enclosed_name()
                    .ok_or_else(|| execution("FrontendPlugin ZIP contains an unsafe path"))?;
                let output = temporary.join(enclosed);
                if file.is_dir() {
                    std::fs::create_dir_all(&output)
                        .map_err(|error| execution(error.to_string()))?;
                    continue;
                }
                if let Some(parent) = output.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| execution(error.to_string()))?;
                }
                let mut target =
                    std::fs::File::create(&output).map_err(|error| execution(error.to_string()))?;
                std::io::copy(&mut file, &mut target)
                    .map_err(|error| execution(error.to_string()))?;
            }
            if !temporary.join(&entrypoint).is_file() {
                return Err(execution(format!(
                    "FrontendPlugin entrypoint {entrypoint:?} does not exist"
                )));
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|error| execution(error.to_string()))?;
            }
            std::fs::rename(&temporary, &destination)
                .map_err(|error| execution(error.to_string()))
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&temporary);
        }
        result
    })
    .await
    .map_err(|error| execution(error.to_string()))?
}

fn safe_relative_path(value: &str) -> Result<PathBuf, GatewayError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(GatewayError::new(
            StatusCode::BAD_REQUEST,
            "invalid plugin asset path",
        ));
    }
    Ok(path.to_owned())
}

fn validate_proxy_prefix(prefix: &str) -> Result<(), DriverError> {
    const RESERVED: [&str; 5] = ["/api", "/gateway", "/plugins", "/assets", "/health"];
    if !prefix.starts_with('/')
        || prefix.len() < 2
        || prefix.ends_with('/')
        || prefix[1..]
            .chars()
            .any(|character| !(character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'))
    {
        return Err(execution(format!("invalid Proxy prefix {prefix:?}")));
    }
    if RESERVED.contains(&prefix) {
        return Err(execution(format!("Proxy prefix {prefix:?} is reserved")));
    }
    Ok(())
}

fn relation_of(resource: &Resource) -> Option<&str> {
    resource.spec.get("relation")?.as_str()
}

fn source_of(resource: &Resource) -> Option<&str> {
    resource.spec.get("source")?.as_str()
}

fn target_of(resource: &Resource) -> Option<&str> {
    resource.spec.get("target")?.as_str()
}

fn status_mutation(resource: &Resource) -> Mutation {
    Mutation::UpdateResourceStatus {
        resource_path: resource.path.clone(),
        expected_revision: resource.revision,
        status: ResourceStatus {
            metadata: resource.status_metadata(resource.metadata.state.clone()),
            spec: resource.spec.clone(),
        },
    }
}

fn normalized_url(value: String) -> String {
    value.trim_end_matches('/').to_owned()
}

fn execution(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}

#[derive(Debug)]
struct GatewayError {
    status: StatusCode,
    message: String,
}

impl GatewayError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

fn internal(error: impl std::fmt::Display) -> GatewayError {
    GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, ZipWriter};

    #[test]
    fn validates_proxy_prefixes_and_segment_matches() {
        assert!(validate_proxy_prefix("/files-api").is_ok());
        assert!(validate_proxy_prefix("/api").is_err());
        assert!(validate_proxy_prefix("/plugins").is_err());
        assert!(validate_proxy_prefix("/UPPER").is_err());
        assert!(prefix_matches("/files-api/files", "/files-api"));
        assert!(!prefix_matches("/files-api-other", "/files-api"));
    }

    #[test]
    fn rejects_unsafe_plugin_asset_paths() {
        assert!(safe_relative_path("assets/app.js").is_ok());
        assert!(safe_relative_path("../secret").is_err());
        assert!(safe_relative_path("/etc/passwd").is_err());
    }

    #[tokio::test]
    async fn rejects_zip_path_traversal() {
        let mut bytes = Vec::new();
        {
            let mut writer = ZipWriter::new(Cursor::new(&mut bytes));
            writer
                .start_file("../index.html", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"not safe").unwrap();
            writer.finish().unwrap();
        }
        let directory = tempfile::tempdir().unwrap();
        let result = extract_plugin(
            bytes,
            directory.path().join("plugin"),
            "index.html".into(),
        )
        .await;
        assert!(result.is_err());
        assert!(!directory.path().join("index.html").exists());
    }
}
