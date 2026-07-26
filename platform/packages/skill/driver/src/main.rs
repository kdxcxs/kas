use std::{env, net::SocketAddr};

use axum::{
    extract::{DefaultBodyLimit, Multipart, Query, State},
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        HeaderMap, Method, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use kas_core::{
    LinkSpec, PlannedResource, PlannedResourceMetadata, Resource, UpdateResource,
    UpdateResourceMetadata,
};
use kas_driver::DriverRuntime;
use kas_skill_driver::{
    validate_bundle, SkillDriver, SkillSpec, BUNDLE_RELATION, LINK_MANIFEST, OWNS_RELATION,
    SKILL_MANIFEST, SKILL_MEDIA_TYPE,
};
use reqwest::{multipart, Client};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

const MAX_BUNDLE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
struct SkillService {
    api: String,
    file_api: String,
    driver_token: String,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct CreateSkillQuery {
    path: String,
}

#[derive(Debug, Deserialize)]
struct UpdateSkillQuery {
    path: String,
    expected_revision: u64,
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
    subject: Subject,
}

#[derive(Debug, Deserialize)]
struct Subject {
    path: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api = env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let file_api = env::var("KAS_FILE_API").unwrap_or_else(|_| "http://127.0.0.1:3001".into());
    let address: SocketAddr = env::var("KAS_SKILL_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:3002".into())
        .parse()?;
    let driver_path = env::var("KAS_DRIVER_PATH")?;
    let generation = env::var("KAS_DRIVER_GENERATION")?.parse()?;
    let token = env::var("KAS_DRIVER_TOKEN")?;
    let listener = TcpListener::bind(address).await?;
    let service = SkillService {
        api: api.trim_end_matches('/').to_owned(),
        file_api: file_api.trim_end_matches('/').to_owned(),
        driver_token: token.clone(),
        client: Client::new(),
    };
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/skills", post(create_skill).patch(update_skill))
        .layer(DefaultBodyLimit::max(MAX_BUNDLE_BYTES))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST, Method::PATCH])
                .allow_headers([AUTHORIZATION, CONTENT_TYPE]),
        )
        .with_state(service);
    let driver = SkillDriver::new(&api, &file_api, &token);
    let runtime = DriverRuntime::new(api, driver_path, generation, token, driver);
    tokio::select! {
        result = axum::serve(listener, app) => result.map_err(Into::into),
        result = runtime.run() => result,
    }
}

async fn create_skill(
    State(service): State<SkillService>,
    headers: HeaderMap,
    Query(query): Query<CreateSkillQuery>,
    multipart: Multipart,
) -> Result<(StatusCode, Json<Resource>), SkillApiError> {
    let authorization = authorization(&headers)?;
    let subject = service
        .authorize(&authorization, "create", &query.path)
        .await?;
    let bytes = bundle_bytes(multipart).await?;
    let validated = validate_bundle(&bytes).map_err(bad_request)?;
    let file_path = bundle_file_path(&query.path);
    let file = service
        .upload_file(&authorization, &file_path, &validated.spec, bytes)
        .await?;
    let skill = PlannedResource {
        metadata: PlannedResourceMetadata {
            path: query.path.clone(),
            manifest: SKILL_MANIFEST.into(),
            name: validated.spec.name.clone(),
            state: "pending".into(),
        },
        spec: serde_json::to_value(&validated.spec).map_err(internal)?,
        status: Default::default(),
    };
    let created = match service.create_resource(&skill).await {
        Ok(resource) => resource,
        Err(error) => {
            let _ = service.delete_resource(&file).await;
            return Err(error);
        }
    };
    let bundle_link = planned_link(
        format!("{}/links/bundle", query.path),
        "bundle",
        BUNDLE_RELATION,
        &query.path,
        &file.path,
        json!({}),
    );
    if let Err(error) = service.create_resource(&bundle_link).await {
        let _ = service.delete_resource(&created).await;
        let _ = service.delete_resource(&file).await;
        return Err(error);
    }
    let owner = owner_path(&subject.path);
    let owner_link = planned_link(
        format!("{}/links/owner", query.path),
        "owner",
        OWNS_RELATION,
        &owner,
        &query.path,
        json!({}),
    );
    if let Err(error) = service.create_resource(&owner_link).await {
        let _ = service.delete_resource(&created).await;
        let _ = service.delete_resource(&file).await;
        return Err(error);
    }
    Ok((StatusCode::CREATED, Json(created)))
}

async fn update_skill(
    State(service): State<SkillService>,
    headers: HeaderMap,
    Query(query): Query<UpdateSkillQuery>,
    multipart: Multipart,
) -> Result<Json<Resource>, SkillApiError> {
    let authorization = authorization(&headers)?;
    service
        .authorize(&authorization, "update", &query.path)
        .await?;
    let current = service.get_resource(&query.path).await?;
    if current.manifest != SKILL_MANIFEST {
        return Err(not_found("Skill"));
    }
    if current.revision != query.expected_revision {
        return Err(SkillApiError(
            StatusCode::CONFLICT,
            "Skill revision is stale".into(),
        ));
    }
    let previous_spec: SkillSpec =
        serde_json::from_value(current.spec.clone()).map_err(internal)?;
    let bytes = bundle_bytes(multipart).await?;
    let validated = validate_bundle(&bytes).map_err(bad_request)?;
    if validated.spec.name != previous_spec.name {
        return Err(bad_request(
            "Skill name is part of its stable identity and cannot be changed",
        ));
    }
    let bundle_link = service
        .get_resource(&format!("{}/links/bundle", query.path))
        .await?;
    let previous_link: LinkSpec =
        serde_json::from_value(bundle_link.spec.clone()).map_err(internal)?;
    if previous_link.relation != BUNDLE_RELATION || previous_link.source != query.path {
        return Err(internal("Skill bundle Link is invalid"));
    }
    let file_path = bundle_file_path(&query.path);
    let new_file = service
        .upload_file(&authorization, &file_path, &validated.spec, bytes)
        .await?;
    let updated_skill = match service
        .update_resource(
            &query.path,
            UpdateResource {
                expected_revision: current.revision,
                metadata: Some(UpdateResourceMetadata {
                    state: "pending".into(),
                }),
                spec: serde_json::to_value(&validated.spec).map_err(internal)?,
            },
        )
        .await
    {
        Ok(resource) => resource,
        Err(error) => {
            let _ = service.delete_resource(&new_file).await;
            return Err(error);
        }
    };
    let replacement = LinkSpec {
        relation: BUNDLE_RELATION.into(),
        source: query.path.clone(),
        target: new_file.path.clone(),
        metadata: json!({}),
    };
    if let Err(error) = service
        .update_resource(
            &bundle_link.path,
            UpdateResource {
                expected_revision: bundle_link.revision,
                metadata: None,
                spec: serde_json::to_value(replacement).map_err(internal)?,
            },
        )
        .await
    {
        let _ = service
            .update_resource(
                &query.path,
                UpdateResource {
                    expected_revision: updated_skill.revision,
                    metadata: Some(UpdateResourceMetadata {
                        state: current.metadata.state.clone(),
                    }),
                    spec: current.spec,
                },
            )
            .await;
        let _ = service.delete_resource(&new_file).await;
        return Err(error);
    }
    if let Ok(previous_file) = service.get_resource(&previous_link.target).await {
        let _ = service.delete_resource(&previous_file).await;
    }
    Ok(Json(updated_skill))
}

impl SkillService {
    async fn authorize(
        &self,
        authorization: &str,
        verb: &str,
        path: &str,
    ) -> Result<Subject, SkillApiError> {
        let response = self
            .client
            .post(format!("{}/auth/check", self.api))
            .header(AUTHORIZATION, authorization)
            .json(&AuthorizationCheck {
                manifest: SKILL_MANIFEST,
                verb,
                path,
            })
            .send()
            .await
            .map_err(internal)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SkillApiError(
                StatusCode::UNAUTHORIZED,
                "authentication required".into(),
            ));
        }
        let decision: AuthorizationDecision = response
            .error_for_status()
            .map_err(upstream)?
            .json()
            .await
            .map_err(internal)?;
        if !decision.allowed {
            return Err(SkillApiError(
                StatusCode::FORBIDDEN,
                "permission denied".into(),
            ));
        }
        Ok(decision.subject)
    }

    async fn upload_file(
        &self,
        authorization: &str,
        path: &str,
        spec: &SkillSpec,
        bytes: Vec<u8>,
    ) -> Result<Resource, SkillApiError> {
        self.client
            .post(format!("{}/files", self.file_api))
            .header(AUTHORIZATION, authorization)
            .query(&[("path", path)])
            .multipart(
                multipart::Form::new().part(
                    "content",
                    multipart::Part::bytes(bytes)
                        .file_name(format!("{}.skill", spec.name))
                        .mime_str(SKILL_MEDIA_TYPE)
                        .map_err(internal)?,
                ),
            )
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(upstream)?
            .json()
            .await
            .map_err(internal)
    }

    async fn create_resource(&self, resource: &PlannedResource) -> Result<Resource, SkillApiError> {
        self.client
            .post(format!("{}/resources", self.api))
            .bearer_auth(&self.driver_token)
            .json(resource)
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(upstream)?
            .json()
            .await
            .map_err(internal)
    }

    async fn get_resource(&self, path: &str) -> Result<Resource, SkillApiError> {
        let response = self
            .client
            .get(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(internal)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(not_found("Resource"));
        }
        response
            .error_for_status()
            .map_err(upstream)?
            .json()
            .await
            .map_err(internal)
    }

    async fn update_resource(
        &self,
        path: &str,
        update: UpdateResource,
    ) -> Result<Resource, SkillApiError> {
        self.client
            .patch(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[("path", path)])
            .json(&update)
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(upstream)?
            .json()
            .await
            .map_err(internal)
    }

    async fn delete_resource(&self, resource: &Resource) -> Result<(), SkillApiError> {
        self.client
            .delete(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[
                ("path", resource.path.as_str()),
                ("expected_revision", &resource.revision.to_string()),
            ])
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(upstream)?;
        Ok(())
    }
}

async fn bundle_bytes(mut multipart: Multipart) -> Result<Vec<u8>, SkillApiError> {
    while let Some(mut field) = multipart.next_field().await.map_err(bad_request)? {
        if field.name() != Some("bundle") {
            continue;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(bad_request)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_BUNDLE_BYTES {
                return Err(SkillApiError(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("Skill bundle exceeds {MAX_BUNDLE_BYTES} bytes"),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(bytes);
    }
    Err(bad_request("multipart field bundle is required"))
}

fn authorization(headers: &HeaderMap) -> Result<String, SkillApiError> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| SkillApiError(StatusCode::UNAUTHORIZED, "authentication required".into()))
}

fn owner_path(subject: &str) -> String {
    subject
        .strip_suffix("/service-account")
        .filter(|path| path.starts_with("/agents/"))
        .unwrap_or(subject)
        .to_owned()
}

fn bundle_file_path(skill_path: &str) -> String {
    format!(
        "/files{}/bundles/{}",
        skill_path.trim_end_matches('/'),
        Uuid::new_v4()
    )
}

fn planned_link(
    path: String,
    name: &str,
    relation: &str,
    source: &str,
    target: &str,
    metadata: serde_json::Value,
) -> PlannedResource {
    PlannedResource {
        metadata: PlannedResourceMetadata {
            path,
            manifest: LINK_MANIFEST.into(),
            name: name.into(),
            state: String::new(),
        },
        spec: serde_json::to_value(LinkSpec {
            relation: relation.into(),
            source: source.into(),
            target: target.into(),
            metadata,
        })
        .expect("Link is serializable"),
        status: Default::default(),
    }
}

fn bad_request(error: impl std::fmt::Display) -> SkillApiError {
    SkillApiError(StatusCode::BAD_REQUEST, error.to_string())
}

fn not_found(kind: &str) -> SkillApiError {
    SkillApiError(StatusCode::NOT_FOUND, format!("{kind} not found"))
}

fn upstream(error: reqwest::Error) -> SkillApiError {
    let status = error
        .status()
        .and_then(|status| StatusCode::from_u16(status.as_u16()).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    SkillApiError(status, error.to_string())
}

fn internal(error: impl std::fmt::Display) -> SkillApiError {
    SkillApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[derive(Debug)]
struct SkillApiError(StatusCode, String);

impl IntoResponse for SkillApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_agent_service_accounts_to_their_agent_owner() {
        assert_eq!(
            owner_path("/agents/reviewer/service-account"),
            "/agents/reviewer"
        );
        assert_eq!(owner_path("/users/admin"), "/users/admin");
    }

    #[test]
    fn stores_each_bundle_as_a_new_file() {
        let first = bundle_file_path("/agents/reviewer/skills/demo");
        let second = bundle_file_path("/agents/reviewer/skills/demo");
        assert!(first.starts_with("/files/agents/reviewer/skills/demo/bundles/"));
        assert_ne!(first, second);
    }
}
