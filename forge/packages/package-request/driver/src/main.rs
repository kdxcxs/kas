use std::{env, net::SocketAddr, path::PathBuf};

use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Query, State},
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        HeaderMap, HeaderValue, Method, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use kas_core::{
    LinkSpec, Mutation, PlannedResource, PlannedResourceMetadata, Resource, ResourceStatus,
    UpdateResource, UpdateResourceMetadata,
};
use kas_driver::{Driver, DriverError, DriverRuntime};
use kas_forge_package_request_driver::{
    digest_hex, inspect, json_error, PackageDecision, PackageRequestSpec, DECIDED_BY_RELATION,
    LINK_MANIFEST, REQUESTED_BY_RELATION, REQUEST_MANIFEST, REQUEST_ROOT, SERVICE_ACCOUNT_MANIFEST,
    USER_MANIFEST,
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

const PACKAGE_MEDIA_TYPE: &str = "application/vnd.kas.manifest+tar";
const MAX_PACKAGE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
struct PackageRequestDriver;

#[async_trait]
impl Driver for PackageRequestDriver {
    fn name(&self) -> &str {
        "forge-package-request"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        if resource.manifest != REQUEST_MANIFEST {
            return Err(DriverError::Execution(format!(
                "Package Request Driver cannot reconcile {}",
                resource.manifest
            )));
        }
        serde_json::from_value::<PackageRequestSpec>(resource.spec.clone()).map_err(|error| {
            DriverError::Execution(format!("invalid Package Request spec: {error}"))
        })?;
        Ok(vec![Mutation::UpdateResourceStatus {
            resource_path: resource.path.clone(),
            expected_revision: resource.revision,
            status: ResourceStatus {
                metadata: resource.status_metadata(resource.metadata.state.clone()),
                spec: resource.spec.clone(),
            },
        }])
    }

    async fn execute(
        &self,
        _resource: &Resource,
        action: &Resource,
        _run: &Resource,
    ) -> Result<kas_core::DriverExecution, DriverError> {
        Err(DriverError::UnsupportedAction(action.path.clone()))
    }
}

#[derive(Clone)]
struct PackageRequestService {
    api: String,
    driver_token: String,
    artifacts: PathBuf,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct AuthContext {
    subject: Subject,
}

#[derive(Debug, Deserialize)]
struct Subject {
    path: String,
    manifest: String,
}

#[derive(Debug, Deserialize)]
struct DecisionQuery {
    path: String,
    expected_revision: u64,
}

#[derive(Debug, Deserialize)]
struct DecideRequest {
    decision: Decision,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Decision {
    Approve,
    Reject,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api = env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let address: SocketAddr = env::var("KAS_PACKAGE_REQUEST_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:3004".into())
        .parse()?;
    let driver_path = env::var("KAS_DRIVER_PATH")?;
    let generation = env::var("KAS_DRIVER_GENERATION")?.parse()?;
    let token = env::var("KAS_DRIVER_TOKEN")?;
    let data_dir = PathBuf::from(env::var("KAS_DATA_DIR")?);
    let artifacts = data_dir.join("forge-package-requests").join("sha256");
    tokio::fs::create_dir_all(&artifacts).await?;

    let listener = TcpListener::bind(address).await?;
    let service = PackageRequestService {
        api: api.trim_end_matches('/').into(),
        driver_token: token.clone(),
        artifacts,
        client: Client::new(),
    };
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/package-requests", post(submit_request))
        .route("/package-requests/decide", post(decide_request))
        .layer(DefaultBodyLimit::max(MAX_PACKAGE_BYTES))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([AUTHORIZATION, CONTENT_TYPE, "x-kas-reason".parse()?]),
        )
        .with_state(service);
    let runtime = DriverRuntime::new(api, driver_path, generation, token, PackageRequestDriver);
    tokio::select! {
        result = axum::serve(listener, app) => result.map_err(anyhow::Error::from),
        result = runtime.run() => result,
    }
}

async fn submit_request(
    State(service): State<PackageRequestService>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Resource>), ApiError> {
    let authorization = authorization(&headers)?;
    let auth = service.auth(authorization).await?;
    if !matches!(
        auth.subject.manifest.as_str(),
        USER_MANIFEST | SERVICE_ACCOUNT_MANIFEST
    ) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "only a User or ServiceAccount may submit a Package Request".into(),
        ));
    }
    let reason = headers
        .get("x-kas-reason")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim();
    if reason.is_empty() || reason.len() > 2048 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "X-KAS-Reason must contain 1 to 2048 bytes".into(),
        ));
    }
    if body.is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "Package is empty".into()));
    }
    let inspection = inspect(&body).map_err(bad_request)?;
    let digest = digest_hex(&inspection.digest).map_err(bad_request)?;
    let artifact_path = service.artifacts.join(format!("{digest}.kas"));
    if !artifact_path.exists() {
        let temporary = service
            .artifacts
            .join(format!(".{digest}-{}.tmp", Uuid::new_v4()));
        tokio::fs::write(&temporary, &body)
            .await
            .map_err(internal)?;
        if let Err(error) = tokio::fs::rename(&temporary, &artifact_path).await {
            if !artifact_path.exists() {
                return Err(internal(error));
            }
            let _ = tokio::fs::remove_file(&temporary).await;
        }
    }

    let id = Uuid::new_v4();
    let path = format!("{REQUEST_ROOT}/requests/{id}");
    let spec = PackageRequestSpec {
        digest: inspection.digest,
        size_bytes: body.len() as u64,
        package_path: inspection.package_path,
        manifest_path: inspection.manifest_path,
        name: inspection.name,
        version: inspection.version,
        description: inspection.description,
        resource_count: inspection.resource_count,
        has_driver: inspection.has_driver,
        reason: reason.into(),
        submitted_at: Utc::now(),
        decision: None,
    };
    let request = service
        .create_resource(planned(
            path.clone(),
            REQUEST_MANIFEST,
            format!("package-request-{id}"),
            serde_json::to_value(spec).map_err(internal)?,
        ))
        .await?;
    service
        .create_resource(link(
            format!("{path}/links/requested-by"),
            REQUESTED_BY_RELATION,
            path,
            auth.subject.path,
        )?)
        .await?;
    Ok((StatusCode::CREATED, Json(request)))
}

async fn decide_request(
    State(service): State<PackageRequestService>,
    headers: HeaderMap,
    Query(query): Query<DecisionQuery>,
    Json(input): Json<DecideRequest>,
) -> Result<Json<Resource>, ApiError> {
    let authorization = authorization(&headers)?;
    let auth = service.auth(authorization).await?;
    if auth.subject.manifest != USER_MANIFEST {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "only a User may decide a Package Request".into(),
        ));
    }
    let mut request = service.resource(&query.path).await?;
    if request.manifest != REQUEST_MANIFEST
        || !request
            .path
            .starts_with(&format!("{REQUEST_ROOT}/requests/"))
    {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "Package Request not found".into(),
        ));
    }
    if request.revision != query.expected_revision {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Package Request revision is stale".into(),
        ));
    }
    if request.metadata.state != "pending" {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("Package Request is already {}", request.metadata.state),
        ));
    }
    let mut spec: PackageRequestSpec =
        serde_json::from_value(request.spec.clone()).map_err(internal)?;
    let decided_at = Utc::now();
    let outcome = match input.decision {
        Decision::Approve => "installing",
        Decision::Reject => "rejected",
    };
    spec.decision = Some(PackageDecision {
        outcome: outcome.into(),
        approver: auth.subject.path.clone(),
        decided_at,
        error: None,
    });
    request = service.update_request(&request, outcome, &spec).await?;
    service
        .create_resource(link(
            format!("{}/links/decided-by", request.path),
            DECIDED_BY_RELATION,
            request.path.clone(),
            auth.subject.path.clone(),
        )?)
        .await?;
    if matches!(input.decision, Decision::Reject) {
        return Ok(Json(request));
    }

    let digest = digest_hex(&spec.digest).map_err(internal)?;
    let artifact = tokio::fs::read(service.artifacts.join(format!("{digest}.kas")))
        .await
        .map_err(internal)?;
    let response = service
        .client
        .post(format!("{}/packages", service.api))
        .header(AUTHORIZATION, authorization)
        .header(CONTENT_TYPE, PACKAGE_MEDIA_TYPE)
        .body(artifact)
        .send()
        .await
        .map_err(forwarded)?;
    let status = response.status();
    let body = response.text().await.map_err(forwarded)?;
    let (final_state, error) = if status.is_success() {
        ("installed", None)
    } else {
        (
            "failed",
            Some(format!(
                "KAS Package installation returned {status}: {body}"
            )),
        )
    };
    spec.decision = Some(PackageDecision {
        outcome: final_state.into(),
        approver: auth.subject.path,
        decided_at,
        error: error.clone(),
    });
    let request = service.update_request(&request, final_state, &spec).await?;
    if let Some(error) = error {
        return Err(ApiError(StatusCode::BAD_REQUEST, error));
    }
    Ok(Json(request))
}

impl PackageRequestService {
    async fn auth(&self, authorization: &HeaderValue) -> Result<AuthContext, ApiError> {
        self.client
            .get(format!("{}/auth", self.api))
            .header(AUTHORIZATION, authorization)
            .send()
            .await
            .map_err(forwarded)?
            .error_for_status()
            .map_err(forwarded)?
            .json()
            .await
            .map_err(forwarded)
    }

    async fn resource(&self, path: &str) -> Result<Resource, ApiError> {
        self.client
            .get(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(forwarded)?
            .error_for_status()
            .map_err(forwarded)?
            .json()
            .await
            .map_err(forwarded)
    }

    async fn create_resource(&self, resource: PlannedResource) -> Result<Resource, ApiError> {
        let response = self
            .client
            .post(format!("{}/resources", self.api))
            .bearer_auth(&self.driver_token)
            .json(&resource)
            .send()
            .await
            .map_err(forwarded)?;
        decode_core_response(response).await
    }

    async fn update_request(
        &self,
        request: &Resource,
        state: &str,
        spec: &PackageRequestSpec,
    ) -> Result<Resource, ApiError> {
        let response = self
            .client
            .patch(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[("path", request.path.as_str())])
            .json(&UpdateResource {
                expected_revision: request.revision,
                metadata: Some(UpdateResourceMetadata {
                    state: state.into(),
                }),
                spec: serde_json::to_value(spec).map_err(internal)?,
            })
            .send()
            .await
            .map_err(forwarded)?;
        decode_core_response(response).await
    }
}

async fn decode_core_response(response: reqwest::Response) -> Result<Resource, ApiError> {
    let status = response.status();
    let body = response.text().await.map_err(forwarded)?;
    if !status.is_success() {
        return Err(ApiError(status, body));
    }
    serde_json::from_str(&body).map_err(internal)
}

fn planned(path: String, manifest: &str, name: String, spec: Value) -> PlannedResource {
    PlannedResource {
        path,
        metadata: PlannedResourceMetadata {
            manifest: manifest.into(),
            name,
            state: String::new(),
        },
        spec,
        status: ResourceStatus::default(),
    }
}

fn link(
    path: String,
    relation: &str,
    source: String,
    target: String,
) -> Result<PlannedResource, ApiError> {
    let name = path.rsplit('/').next().unwrap_or("link").to_owned();
    Ok(planned(
        path,
        LINK_MANIFEST,
        name,
        serde_json::to_value(LinkSpec {
            relation: relation.into(),
            source,
            target,
            metadata: json!({}),
        })
        .map_err(internal)?,
    ))
}

fn authorization(headers: &HeaderMap) -> Result<&HeaderValue, ApiError> {
    headers
        .get(AUTHORIZATION)
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "missing Authorization".into()))
}

fn bad_request(error: impl ToString) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, error.to_string())
}

fn internal(error: impl ToString) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn forwarded(error: reqwest::Error) -> ApiError {
    ApiError(
        error.status().unwrap_or(StatusCode::BAD_GATEWAY),
        error.to_string(),
    )
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json_error(self.1))).into_response()
    }
}
