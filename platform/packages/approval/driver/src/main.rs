use std::{env, net::SocketAddr};

use axum::{
    extract::{Query, State},
    http::{header::AUTHORIZATION, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{Duration, Utc};
use kas_approval_driver::{
    ApprovalDriver, ApprovalOperation, ApprovalResponse, ApprovalResultSpec, ApprovalSpec,
    DecisionOutcome, APPROVAL_MANIFEST, APPROVAL_RESULT_MANIFEST, DECIDED_BY_RELATION,
    DECIDES_RELATION, LINK_MANIFEST, PRODUCED_BY_RELATION, REQUESTED_BY_RELATION,
    RESULT_OF_RELATION, SERVICE_ACCOUNT_MANIFEST, USER_MANIFEST,
};
use kas_core::{
    LinkSpec, PlannedResource, PlannedResourceMetadata, Resource, ResourceStatus, UpdateResource,
    UpdateResourceMetadata,
};
use kas_driver::DriverRuntime;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

#[derive(Clone)]
struct ApprovalService {
    api: String,
    driver_token: String,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct RequestApproval {
    reason: String,
    operation: ApprovalOperation,
    #[serde(default = "default_expiry_seconds")]
    expires_in_seconds: i64,
}

#[derive(Debug, Deserialize)]
struct DecideQuery {
    path: String,
    expected_revision: u64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Decision {
    Approve,
    Reject,
}

#[derive(Debug, Deserialize)]
struct DecideApproval {
    decision: Decision,
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

#[derive(Debug, Serialize)]
struct AuthorizationCheck<'a> {
    manifest: &'a str,
    verb: &'a str,
    path: &'a str,
}

#[derive(Debug, Deserialize)]
struct AuthorizationDecision {
    allowed: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api = env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let address: SocketAddr = env::var("KAS_APPROVAL_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:3003".into())
        .parse()?;
    let driver_path = env::var("KAS_DRIVER_PATH")?;
    let generation = env::var("KAS_DRIVER_GENERATION")?.parse()?;
    let token = env::var("KAS_DRIVER_TOKEN")?;
    let listener = TcpListener::bind(address).await?;
    let service = ApprovalService {
        api: api.trim_end_matches('/').to_owned(),
        driver_token: token.clone(),
        client: Client::new(),
    };
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/approvals", post(request_approval))
        .route("/approvals/decide", post(decide_approval))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([AUTHORIZATION, axum::http::header::CONTENT_TYPE]),
        )
        .with_state(service);
    let driver = ApprovalDriver::new(&api, &token);
    let runtime = DriverRuntime::new(api, driver_path, generation, token, driver);
    tokio::select! {
        result = axum::serve(listener, app) => result.map_err(Into::into),
        result = run_runtime(runtime) => result,
    }
}

async fn run_runtime(runtime: DriverRuntime<ApprovalDriver>) -> anyhow::Result<()> {
    loop {
        match runtime.run().await {
            Err(error)
                if error
                    .to_string()
                    .contains("mutation was rejected (conflict)") =>
            {
                eprintln!("Approval reconciliation conflicted with an API update; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            result => return result,
        }
    }
}

async fn request_approval(
    State(service): State<ApprovalService>,
    headers: HeaderMap,
    Json(input): Json<RequestApproval>,
) -> Result<(StatusCode, Json<Resource>), ApprovalApiError> {
    let authorization = authorization(&headers)?;
    let auth = service.auth(&authorization).await?;
    let requester = requester_path(&auth.subject)?;
    let reason = input.reason.trim();
    if reason.is_empty() || reason.len() > 2048 {
        return Err(bad_request("reason must contain 1 to 2048 bytes"));
    }
    if !(30..=3600).contains(&input.expires_in_seconds) {
        return Err(bad_request(
            "expires_in_seconds must be between 30 and 3600",
        ));
    }
    if !input.operation.scope_path().starts_with('/') {
        return Err(bad_request("operation path must be absolute"));
    }
    if let ApprovalOperation::List {
        manifest, limit, ..
    } = &input.operation
    {
        if !manifest.starts_with('/') {
            return Err(bad_request("list manifest must be absolute"));
        }
        if !(1..=1000).contains(limit) {
            return Err(bad_request("list limit must be between 1 and 1000"));
        }
    }
    let path = format!("/approvals{requester}/requests/{}", Uuid::new_v4());
    let expires_at = Utc::now() + Duration::seconds(input.expires_in_seconds);
    let spec = ApprovalSpec::Request {
        reason: reason.into(),
        operation: input.operation,
        expires_at,
    };
    let approval = service
        .create_resource(planned(
            path.clone(),
            "approval-request",
            "pending",
            serde_json::to_value(spec).map_err(internal)?,
        ))
        .await?;
    let requester_link = planned_link(
        format!("{path}/links/requested-by"),
        REQUESTED_BY_RELATION,
        path.clone(),
        requester,
    );
    if let Err(error) = service.create_resource(requester_link).await {
        let _ = service.delete_driver_resource(&approval).await;
        return Err(error);
    }
    Ok((StatusCode::CREATED, Json(approval)))
}

async fn decide_approval(
    State(service): State<ApprovalService>,
    headers: HeaderMap,
    Query(query): Query<DecideQuery>,
    Json(input): Json<DecideApproval>,
) -> Result<Json<Resource>, ApprovalApiError> {
    let authorization = authorization(&headers)?;
    let auth = service.auth(&authorization).await?;
    if auth.subject.manifest != USER_MANIFEST {
        return Err(ApprovalApiError(
            StatusCode::FORBIDDEN,
            "only a User may decide an Approval".into(),
        ));
    }
    let approval = service.get_resource(&query.path).await?;
    if approval.manifest != APPROVAL_MANIFEST {
        return Err(not_found("Approval"));
    }
    if approval.revision != query.expected_revision {
        return Err(ApprovalApiError(
            StatusCode::CONFLICT,
            "Approval revision is stale".into(),
        ));
    }
    let ApprovalSpec::Request {
        operation,
        expires_at,
        ..
    } = serde_json::from_value::<ApprovalSpec>(approval.spec.clone()).map_err(internal)?
    else {
        return Err(bad_request("only an Approval request may be decided"));
    };
    if expires_at <= Utc::now() {
        return Err(ApprovalApiError(
            StatusCode::CONFLICT,
            "Approval has expired".into(),
        ));
    }
    let requester = service.requester_for(&approval.path).await?;
    let path = format!(
        "/approvals{}/decisions/{}",
        auth.subject.path,
        Uuid::new_v4()
    );
    let decided_at = Utc::now();
    let decision = service
        .create_resource(planned(
            path.clone(),
            "approval-decision",
            "pending",
            serde_json::to_value(ApprovalSpec::Decision {
                outcome: DecisionOutcome::Pending,
                decided_at,
                completed_at: None,
                error: None,
            })
            .map_err(internal)?,
        ))
        .await?;
    if let Err(error) = service
        .create_decision_links(&decision.path, &approval.path, &auth.subject.path)
        .await
    {
        let _ = service.delete_driver_resource(&decision).await;
        return Err(error);
    }
    let allowed = match service.check_operation(&authorization, &operation).await {
        Ok(allowed) => allowed,
        Err(error) => {
            let failed = service
                .finish_decision(
                    &decision,
                    decided_at,
                    DecisionOutcome::Failed,
                    Some(error.1.clone()),
                )
                .await?;
            return Ok(Json(failed));
        }
    };
    if !allowed {
        let invalid = service
            .finish_decision(
                &decision,
                decided_at,
                DecisionOutcome::Invalid,
                Some("approver is not authorized for the requested operation".into()),
            )
            .await?;
        return Ok(Json(invalid));
    }

    let claim_state = match input.decision {
        Decision::Approve => "executing",
        Decision::Reject => "rejected",
    };
    let claimed = match service.claim_request(&approval, claim_state).await {
        Ok(claimed) => claimed,
        Err(error) if error.0 == StatusCode::CONFLICT => {
            let superseded = service
                .finish_decision(
                    &decision,
                    decided_at,
                    DecisionOutcome::Superseded,
                    Some("another valid Decision already claimed this Request".into()),
                )
                .await?;
            return Ok(Json(superseded));
        }
        Err(error) => return Err(error),
    };
    if matches!(input.decision, Decision::Reject) {
        let rejected = service
            .finish_decision(&decision, decided_at, DecisionOutcome::Rejected, None)
            .await?;
        return Ok(Json(rejected));
    }

    service
        .wait_for_status(&claimed.path, "executing", claimed.revision)
        .await?;
    let executing = service
        .finish_decision(&decision, decided_at, DecisionOutcome::Executing, None)
        .await?;
    service
        .wait_for_status(&executing.path, "executing", executing.revision)
        .await?;
    let execution = service.execute(&authorization, &operation).await;
    let (mut outcome, mut execution_error) = match execution {
        Ok(response) => match service
            .create_result(&requester, &approval.path, &executing.path, response)
            .await
        {
            Ok(_) => (DecisionOutcome::Succeeded, None),
            Err(error) => (
                DecisionOutcome::Failed,
                Some(format!(
                    "operation succeeded but Result creation failed: {}",
                    error.1
                )),
            ),
        },
        Err(error) => (DecisionOutcome::Failed, Some(error.1)),
    };
    if let Err(error) = service.finish_request(&claimed, outcome.state()).await {
        outcome = DecisionOutcome::Failed;
        execution_error = Some(format!(
            "{}Request finalization failed: {}",
            execution_error
                .as_deref()
                .map(|message| format!("{message}; "))
                .unwrap_or_default(),
            error.1
        ));
    }
    let completed = service
        .finish_decision(&executing, decided_at, outcome, execution_error)
        .await?;
    Ok(Json(completed))
}

impl ApprovalService {
    async fn auth(&self, authorization: &HeaderValue) -> Result<AuthContext, ApprovalApiError> {
        self.client
            .get(format!("{}/auth", self.api))
            .header(AUTHORIZATION, authorization.clone())
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(forwarded)?
            .json()
            .await
            .map_err(internal)
    }

    async fn get_resource(&self, path: &str) -> Result<Resource, ApprovalApiError> {
        self.client
            .get(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(forwarded)?
            .json()
            .await
            .map_err(internal)
    }

    async fn create_resource(
        &self,
        resource: PlannedResource,
    ) -> Result<Resource, ApprovalApiError> {
        let response = self
            .client
            .post(format!("{}/resources", self.api))
            .bearer_auth(&self.driver_token)
            .json(&resource)
            .send()
            .await
            .map_err(internal)?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ApprovalApiError(status, body));
        }
        response.json().await.map_err(internal)
    }

    async fn list_resources(&self, manifest: &str) -> Result<Vec<Resource>, ApprovalApiError> {
        self.client
            .get(format!("{}/resources", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[("manifest", manifest)])
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(forwarded)?
            .json()
            .await
            .map_err(internal)
    }

    async fn requester_for(&self, request: &str) -> Result<String, ApprovalApiError> {
        self.list_resources(LINK_MANIFEST)
            .await?
            .into_iter()
            .find_map(|resource| {
                let link = serde_json::from_value::<LinkSpec>(resource.spec).ok()?;
                (link.relation == REQUESTED_BY_RELATION && link.source == request)
                    .then_some(link.target)
            })
            .ok_or_else(|| bad_request("Approval Request has no requested-by Link"))
    }

    async fn create_decision_links(
        &self,
        decision: &str,
        request: &str,
        approver: &str,
    ) -> Result<(), ApprovalApiError> {
        self.create_resource(planned_link(
            format!("{decision}/links/request"),
            DECIDES_RELATION,
            decision.into(),
            request.into(),
        ))
        .await?;
        if let Err(error) = self
            .create_resource(planned_link(
                format!("{decision}/links/decided-by"),
                DECIDED_BY_RELATION,
                decision.into(),
                approver.into(),
            ))
            .await
        {
            return Err(error);
        }
        Ok(())
    }

    async fn check_operation(
        &self,
        authorization: &HeaderValue,
        operation: &ApprovalOperation,
    ) -> Result<bool, ApprovalApiError> {
        let (manifest, verb, path) = match operation {
            ApprovalOperation::Create { resource } => (
                resource.metadata.manifest.clone(),
                "create",
                resource.metadata.path.clone(),
            ),
            ApprovalOperation::List {
                manifest,
                path_prefix,
                ..
            } => (
                manifest.clone(),
                "list",
                authorization_probe_path(path_prefix.as_deref()),
            ),
            ApprovalOperation::Update { path, .. } => (
                self.get_resource(path).await?.manifest.clone(),
                "update",
                path.clone(),
            ),
            ApprovalOperation::Delete { path, .. } => (
                self.get_resource(path).await?.manifest.clone(),
                "delete",
                path.clone(),
            ),
            ApprovalOperation::Get { path } => (
                self.get_resource(path).await?.manifest.clone(),
                "get",
                path.clone(),
            ),
        };
        let decision: AuthorizationDecision = self
            .client
            .post(format!("{}/auth/check", self.api))
            .header(AUTHORIZATION, authorization.clone())
            .json(&AuthorizationCheck {
                manifest: &manifest,
                verb,
                path: &path,
            })
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(forwarded)?
            .json()
            .await
            .map_err(internal)?;
        Ok(decision.allowed)
    }

    async fn claim_request(
        &self,
        request: &Resource,
        state: &str,
    ) -> Result<Resource, ApprovalApiError> {
        if request.metadata.state != "pending" {
            return Err(ApprovalApiError(
                StatusCode::CONFLICT,
                format!("Approval Request is already {}", request.metadata.state),
            ));
        }
        self.update_resource_state(request, state).await
    }

    async fn finish_request(
        &self,
        request: &Resource,
        state: &str,
    ) -> Result<Resource, ApprovalApiError> {
        self.update_resource_state(request, state).await
    }

    async fn wait_for_status(
        &self,
        path: &str,
        state: &str,
        revision: u64,
    ) -> Result<Resource, ApprovalApiError> {
        for _ in 0..250 {
            let resource = self.get_resource(path).await?;
            if resource.revision != revision {
                return Err(ApprovalApiError(
                    StatusCode::CONFLICT,
                    format!("Resource {path} changed while waiting for reconciliation"),
                ));
            }
            if resource.status.metadata.state == state
                && resource.status.metadata.revision == revision
            {
                return Ok(resource);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Err(ApprovalApiError(
            StatusCode::GATEWAY_TIMEOUT,
            format!("Resource {path} was not reconciled in time"),
        ))
    }

    async fn update_resource_state(
        &self,
        resource: &Resource,
        state: &str,
    ) -> Result<Resource, ApprovalApiError> {
        self.client
            .patch(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[("path", resource.path.as_str())])
            .json(&UpdateResource {
                expected_revision: resource.revision,
                metadata: Some(UpdateResourceMetadata {
                    state: state.into(),
                }),
                spec: resource.spec.clone(),
            })
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(forwarded)?
            .json()
            .await
            .map_err(internal)
    }

    async fn create_result(
        &self,
        requester: &str,
        request: &str,
        decision: &str,
        response: ApprovalResponse,
    ) -> Result<Resource, ApprovalApiError> {
        let path = format!("/approvals{requester}/results/{}", Uuid::new_v4());
        let result = self
            .create_resource(planned_for(
                path.clone(),
                APPROVAL_RESULT_MANIFEST,
                "approval-result",
                "available",
                serde_json::to_value(ApprovalResultSpec { response }).map_err(internal)?,
            ))
            .await?;
        for (name, relation, target) in [
            ("request", RESULT_OF_RELATION, request),
            ("decision", PRODUCED_BY_RELATION, decision),
        ] {
            if let Err(error) = self
                .create_resource(planned_link(
                    format!("{path}/links/{name}"),
                    relation,
                    path.clone(),
                    target.into(),
                ))
                .await
            {
                let _ = self.delete_driver_resource(&result).await;
                return Err(error);
            }
        }
        Ok(result)
    }

    async fn execute(
        &self,
        authorization: &HeaderValue,
        operation: &ApprovalOperation,
    ) -> Result<ApprovalResponse, ApprovalApiError> {
        let response = match operation {
            ApprovalOperation::Create { resource } => {
                self.client
                    .post(format!("{}/resources", self.api))
                    .header(AUTHORIZATION, authorization.clone())
                    .json(resource)
                    .send()
                    .await
            }
            ApprovalOperation::Update { path, update } => {
                self.client
                    .patch(format!("{}/resources/by-path", self.api))
                    .header(AUTHORIZATION, authorization.clone())
                    .query(&[("path", path)])
                    .json(update)
                    .send()
                    .await
            }
            ApprovalOperation::Delete {
                path,
                expected_revision,
            } => {
                let expected_revision = expected_revision.to_string();
                self.client
                    .delete(format!("{}/resources/by-path", self.api))
                    .header(AUTHORIZATION, authorization.clone())
                    .query(&[
                        ("path", path.as_str()),
                        ("expected_revision", expected_revision.as_str()),
                    ])
                    .send()
                    .await
            }
            ApprovalOperation::Get { path } => {
                self.client
                    .get(format!("{}/resources/by-path", self.api))
                    .header(AUTHORIZATION, authorization.clone())
                    .query(&[("path", path)])
                    .send()
                    .await
            }
            ApprovalOperation::List { manifest, .. } => {
                self.client
                    .get(format!("{}/resources", self.api))
                    .header(AUTHORIZATION, authorization.clone())
                    .query(&[("manifest", manifest)])
                    .send()
                    .await
            }
        }
        .map_err(internal)?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/json")
            .to_owned();
        let response = response.error_for_status().map_err(forwarded)?;
        let body = match operation {
            ApprovalOperation::List {
                path_prefix, limit, ..
            } => {
                let resources: Vec<Resource> = response.json().await.map_err(internal)?;
                Value::Array(
                    resources
                        .into_iter()
                        .filter(|resource| {
                            path_prefix
                                .as_ref()
                                .is_none_or(|prefix| resource.path.starts_with(prefix))
                        })
                        .take(*limit as usize)
                        .map(safe_resource)
                        .collect(),
                )
            }
            _ => safe_resource(response.json().await.map_err(internal)?),
        };
        Ok(ApprovalResponse {
            status: status.as_u16(),
            content_type,
            body,
        })
    }

    async fn delete_driver_resource(&self, resource: &Resource) -> Result<(), ApprovalApiError> {
        let revision = resource.revision.to_string();
        self.client
            .delete(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[
                ("path", resource.path.as_str()),
                ("expected_revision", revision.as_str()),
            ])
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(forwarded)?;
        Ok(())
    }

    async fn finish_decision(
        &self,
        decision: &Resource,
        decided_at: chrono::DateTime<Utc>,
        outcome: DecisionOutcome,
        error: Option<String>,
    ) -> Result<Resource, ApprovalApiError> {
        let update = UpdateResource {
            expected_revision: decision.revision,
            metadata: Some(UpdateResourceMetadata {
                state: outcome.state().into(),
            }),
            spec: serde_json::to_value(ApprovalSpec::Decision {
                outcome,
                decided_at,
                completed_at: (!matches!(
                    outcome,
                    DecisionOutcome::Pending | DecisionOutcome::Executing
                ))
                .then(Utc::now),
                error,
            })
            .map_err(internal)?,
        };
        self.client
            .patch(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[("path", decision.path.as_str())])
            .json(&update)
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(forwarded)?
            .json()
            .await
            .map_err(internal)
    }
}

fn requester_path(subject: &Subject) -> Result<String, ApprovalApiError> {
    if subject.manifest == USER_MANIFEST {
        return Ok(subject.path.clone());
    }
    if subject.manifest == SERVICE_ACCOUNT_MANIFEST {
        if let Some(agent) = subject.path.strip_suffix("/service-account") {
            if agent.starts_with("/agents/") {
                return Ok(agent.into());
            }
        }
    }
    Err(ApprovalApiError(
        StatusCode::FORBIDDEN,
        "only a User or Agent ServiceAccount may request approval".into(),
    ))
}

fn planned(path: String, name: &str, state: &str, spec: Value) -> PlannedResource {
    planned_for(path, APPROVAL_MANIFEST, name, state, spec)
}

fn planned_for(
    path: String,
    manifest: &str,
    name: &str,
    state: &str,
    spec: Value,
) -> PlannedResource {
    PlannedResource {
        metadata: PlannedResourceMetadata {
            path,
            manifest: manifest.into(),
            name: name.into(),
            state: state.into(),
        },
        spec,
        status: ResourceStatus::default(),
    }
}

fn safe_resource(resource: Resource) -> Value {
    json!({
        "metadata": {
            "path": resource.path,
            "manifest": resource.manifest,
            "name": resource.name,
            "state": resource.metadata.state,
            "revision": resource.revision,
            "created_at": resource.created_at,
            "updated_at": resource.updated_at
        },
        "spec": resource.spec,
        "status": {
            "metadata": {
                "path": resource.status.metadata.path,
                "manifest": resource.status.metadata.manifest,
                "name": resource.status.metadata.name,
                "state": resource.status.metadata.state,
                "revision": resource.status.metadata.revision,
                "created_at": resource.status.metadata.created_at,
                "updated_at": resource.status.metadata.updated_at
            },
            "spec": resource.status.spec
        }
    })
}

fn planned_link(path: String, relation: &str, source: String, target: String) -> PlannedResource {
    PlannedResource {
        metadata: PlannedResourceMetadata {
            name: path.rsplit('/').next().unwrap_or("link").into(),
            path,
            manifest: LINK_MANIFEST.into(),
            state: String::new(),
        },
        spec: serde_json::to_value(LinkSpec {
            relation: relation.into(),
            source,
            target,
            metadata: json!({}),
        })
        .expect("Link spec is serializable"),
        status: ResourceStatus::default(),
    }
}

fn authorization(headers: &HeaderMap) -> Result<HeaderValue, ApprovalApiError> {
    headers
        .get(AUTHORIZATION)
        .cloned()
        .ok_or_else(|| ApprovalApiError(StatusCode::UNAUTHORIZED, "missing Authorization".into()))
}

fn default_expiry_seconds() -> i64 {
    900
}

fn authorization_probe_path(path_prefix: Option<&str>) -> String {
    match path_prefix {
        Some(prefix) if prefix != "/" => prefix.trim_end_matches('/').into(),
        _ => "/__kas_list_scope__".into(),
    }
}

fn bad_request(error: impl ToString) -> ApprovalApiError {
    ApprovalApiError(StatusCode::BAD_REQUEST, error.to_string())
}

fn not_found(name: &str) -> ApprovalApiError {
    ApprovalApiError(StatusCode::NOT_FOUND, format!("{name} was not found"))
}

fn internal(error: impl ToString) -> ApprovalApiError {
    ApprovalApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn forwarded(error: reqwest::Error) -> ApprovalApiError {
    ApprovalApiError(
        error.status().unwrap_or(StatusCode::BAD_GATEWAY),
        error.to_string(),
    )
}

#[derive(Debug)]
struct ApprovalApiError(StatusCode, String);

impl IntoResponse for ApprovalApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::{authorization_probe_path, requester_path, Subject};
    use kas_approval_driver::{SERVICE_ACCOUNT_MANIFEST, USER_MANIFEST};

    #[test]
    fn maps_agent_service_account_to_agent() {
        assert_eq!(
            requester_path(&Subject {
                path: "/agents/reviewer/service-account".into(),
                manifest: SERVICE_ACCOUNT_MANIFEST.into(),
            })
            .unwrap(),
            "/agents/reviewer"
        );
    }

    #[test]
    fn keeps_user_as_requester() {
        assert_eq!(
            requester_path(&Subject {
                path: "/users/alice".into(),
                manifest: USER_MANIFEST.into(),
            })
            .unwrap(),
            "/users/alice"
        );
    }

    #[test]
    fn list_authorization_uses_a_valid_scope_path() {
        assert_eq!(
            authorization_probe_path(Some("/approval-proofs/")),
            "/approval-proofs"
        );
        assert_eq!(authorization_probe_path(Some("/")), "/__kas_list_scope__");
        assert_eq!(authorization_probe_path(None), "/__kas_list_scope__");
    }
}
