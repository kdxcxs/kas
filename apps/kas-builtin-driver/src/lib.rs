//! Built-in controllers that keep ordinary Relation and Link Resources valid.

use async_trait::async_trait;
use kas_core::{
    DriverExecution, LinkSpec, Mutation, OnSourceDelete, RelationSpec, Resource, ResourceStatus,
    STATE_AVAILABLE, STATE_DELETED,
};
use kas_driver::{Driver, DriverError, DriverRuntime};

const RELATION_MANIFEST: &str = "/builtin/relation";
const LINK_MANIFEST: &str = "/builtin/link";
const INVALID_STATE: &str = "invalid";

pub async fn run_builtin_driver() -> anyhow::Result<()> {
    let api = std::env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let driver_path = std::env::var("KAS_DRIVER_PATH")?;
    let generation = std::env::var("KAS_DRIVER_GENERATION")?.parse()?;
    let token = std::env::var("KAS_DRIVER_TOKEN")?;
    let driver = RelationshipDriver::new(api.clone(), token.clone());
    DriverRuntime::new(api, driver_path, generation, token, driver)
        .run()
        .await
}

struct RelationshipDriver {
    api: String,
    token: String,
    client: reqwest::Client,
}

impl RelationshipDriver {
    fn new(api: String, token: String) -> Self {
        Self {
            api: api.trim_end_matches('/').into(),
            token,
            client: reqwest::Client::new(),
        }
    }

    async fn resource(&self, path: &str) -> Result<Option<Resource>, DriverError> {
        let mut url =
            reqwest::Url::parse(&format!("{}/resources/by-path", self.api)).map_err(execution)?;
        url.query_pairs_mut().append_pair("path", path);
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(execution)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(
            response
                .error_for_status()
                .map_err(execution)?
                .json()
                .await
                .map_err(execution)?,
        ))
    }

    fn status_for(
        link: &Resource,
        relation: Option<&Resource>,
        source: Option<&Resource>,
        target: Option<&Resource>,
    ) -> ResourceStatus {
        if link.metadata.state == STATE_DELETED {
            return ResourceStatus {
                metadata: link.status_metadata(STATE_DELETED),
                spec: link.spec.clone(),
            };
        }
        match validate_link(link, relation, source, target) {
            Ok(()) => ResourceStatus {
                metadata: link.status_metadata(STATE_AVAILABLE),
                spec: link.spec.clone(),
            },
            Err(_) => ResourceStatus {
                metadata: link.status_metadata(INVALID_STATE),
                spec: link.spec.clone(),
            },
        }
    }
}

#[async_trait]
impl Driver for RelationshipDriver {
    fn name(&self) -> &str {
        "builtin-relationship-driver"
    }

    async fn reconcile(&self, delivered: &Resource) -> Result<Vec<Mutation>, DriverError> {
        if delivered.manifest == RELATION_MANIFEST {
            let status = relation_status(delivered);
            if delivered.status != status {
                return Ok(vec![Mutation::UpdateResourceStatus {
                    resource_path: delivered.path.clone(),
                    expected_revision: delivered.revision,
                    status,
                }]);
            }
            return Ok(Vec::new());
        }
        if delivered.manifest != LINK_MANIFEST {
            return Ok(Vec::new());
        }

        if delivered.metadata.state == STATE_DELETED {
            let status = ResourceStatus {
                metadata: delivered.status_metadata(STATE_DELETED),
                spec: delivered.spec.clone(),
            };
            return Ok((delivered.status != status)
                .then_some(Mutation::UpdateResourceStatus {
                    resource_path: delivered.path.clone(),
                    expected_revision: delivered.revision,
                    status,
                })
                .into_iter()
                .collect());
        }

        let Ok(spec) = serde_json::from_value::<LinkSpec>(delivered.spec.clone()) else {
            return Ok(status_mutation(delivered, INVALID_STATE)
                .into_iter()
                .collect());
        };
        let relation = self.resource(&spec.relation).await?;
        let source = self.resource(&spec.source).await?;
        let target = self.resource(&spec.target).await?;
        let dependency_deleted = [&relation, &source, &target].into_iter().any(|resource| {
            resource
                .as_ref()
                .is_none_or(|resource| resource.metadata.state == STATE_DELETED)
        });
        if dependency_deleted {
            let mut operations = vec![
                Mutation::DeleteResource {
                    resource_path: delivered.path.clone(),
                    expected_revision: delivered.revision,
                },
                Mutation::UpdateResourceStatus {
                    resource_path: delivered.path.clone(),
                    expected_revision: delivered.revision + 1,
                    status: ResourceStatus {
                        metadata: delivered.status_metadata(STATE_DELETED),
                        spec: delivered.spec.clone(),
                    },
                },
            ];
            if source
                .as_ref()
                .is_some_and(|source| source.metadata.state == STATE_DELETED)
                && relation.as_ref().is_some_and(|relation| {
                    serde_json::from_value::<RelationSpec>(relation.spec.clone())
                        .is_ok_and(|relation| relation.on_source_delete == OnSourceDelete::Cascade)
                })
            {
                if let Some(target) = target.filter(|target| target.metadata.state != STATE_DELETED)
                {
                    let expected_revision = target.revision;
                    operations.push(Mutation::DeleteResource {
                        resource_path: target.path,
                        expected_revision,
                    });
                }
            }
            return Ok(operations);
        }

        let status = Self::status_for(
            delivered,
            relation.as_ref(),
            source.as_ref(),
            target.as_ref(),
        );
        let mut operations = Vec::new();
        if delivered.status != status {
            operations.push(Mutation::UpdateResourceStatus {
                resource_path: delivered.path.clone(),
                expected_revision: delivered.revision,
                status,
            });
        }
        Ok(operations)
    }

    async fn execute(
        &self,
        _: &Resource,
        action: &Resource,
        _: &Resource,
    ) -> Result<DriverExecution, DriverError> {
        Err(DriverError::UnsupportedAction(action.path.clone()))
    }
}

fn validate_relation(resource: &Resource) -> Result<(), String> {
    let spec: RelationSpec = serde_json::from_value(resource.spec.clone())
        .map_err(|error| format!("Invalid Relation: {error}"))?;
    if spec.sources.is_empty() || spec.targets.is_empty() {
        return Err("Relation sources and targets must not be empty".into());
    }
    jsonschema::validator_for(&spec.metadata_schema)
        .map_err(|error| format!("Relation metadata schema is invalid: {error}"))?;
    Ok(())
}

fn relation_status(resource: &Resource) -> ResourceStatus {
    if resource.metadata.state == STATE_DELETED {
        return ResourceStatus {
            metadata: resource.status_metadata(STATE_DELETED),
            spec: resource.spec.clone(),
        };
    }
    match validate_relation(resource) {
        Ok(()) => ResourceStatus {
            metadata: resource.status_metadata(STATE_AVAILABLE),
            spec: resource.spec.clone(),
        },
        Err(_) => ResourceStatus {
            metadata: resource.status_metadata(INVALID_STATE),
            spec: resource.spec.clone(),
        },
    }
}

fn validate_link(
    link: &Resource,
    relation: Option<&Resource>,
    source: Option<&Resource>,
    target: Option<&Resource>,
) -> Result<(), String> {
    let spec: LinkSpec =
        serde_json::from_value(link.spec.clone()).map_err(|error| error.to_string())?;
    let relation = relation.ok_or_else(|| format!("Relation {} does not exist", spec.relation))?;
    if relation.manifest != RELATION_MANIFEST {
        return Err(format!("{} is not a Relation", relation.path));
    }
    let relation_spec: RelationSpec =
        serde_json::from_value(relation.spec.clone()).map_err(|error| error.to_string())?;
    let source = source.ok_or_else(|| format!("Source {} does not exist", spec.source))?;
    let target = target.ok_or_else(|| format!("Target {} does not exist", spec.target))?;
    if !relation_spec
        .sources
        .iter()
        .any(|selector| selector.matches(source))
    {
        return Err(format!(
            "Source {} is not accepted by Relation {}",
            source.path, relation.path
        ));
    }
    if !relation_spec
        .targets
        .iter()
        .any(|selector| selector.matches(target))
    {
        return Err(format!(
            "Target {} is not accepted by Relation {}",
            target.path, relation.path
        ));
    }
    let validator = jsonschema::validator_for(&relation_spec.metadata_schema)
        .map_err(|error| format!("Link metadata schema is invalid: {error}"))?;
    validator
        .validate(&spec.metadata)
        .map_err(|error| format!("Link metadata is invalid: {error}"))
}

fn status_mutation(link: &Resource, state: &str) -> Option<Mutation> {
    let status = ResourceStatus {
        metadata: link.status_metadata(state),
        spec: link.spec.clone(),
    };
    (link.status != status).then_some(Mutation::UpdateResourceStatus {
        resource_path: link.path.clone(),
        expected_revision: link.revision,
        status,
    })
}

fn execution(error: impl std::fmt::Display) -> DriverError {
    DriverError::Execution(error.to_string())
}
