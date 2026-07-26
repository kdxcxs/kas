//! Built-in controllers that keep ordinary Relation and Link Resources valid.

use std::collections::{BTreeMap, BTreeSet};

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

    async fn resources(&self) -> Result<Vec<Resource>, DriverError> {
        self.client
            .get(format!("{}/resources", self.api))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(execution)?
            .error_for_status()
            .map_err(execution)?
            .json()
            .await
            .map_err(execution)
    }

    fn status_for(link: &Resource, resources: &BTreeMap<String, Resource>) -> ResourceStatus {
        if link.metadata.state == STATE_DELETED {
            return ResourceStatus {
                metadata: link.status_metadata(STATE_DELETED),
                spec: link.spec.clone(),
            };
        }
        match validate_link(link, resources) {
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
        let resources = self.resources().await?;
        let by_path = resources
            .into_iter()
            .map(|resource| (resource.path.clone(), resource))
            .collect::<BTreeMap<_, _>>();
        let mut operations = Vec::new();
        let mut scheduled_deletions = BTreeSet::new();
        if delivered.manifest == RELATION_MANIFEST {
            let status = relation_status(delivered);
            if delivered.status != status {
                operations.push(Mutation::UpdateResourceStatus {
                    resource_path: delivered.path.clone(),
                    expected_revision: delivered.revision,
                    status,
                });
            }
        }
        for link in by_path
            .values()
            .filter(|resource| resource.manifest == LINK_MANIFEST)
        {
            let decoded = serde_json::from_value::<LinkSpec>(link.spec.clone());
            let affected = link.path == delivered.path
                || decoded.as_ref().is_ok_and(|spec| {
                    spec.relation == delivered.path
                        || spec.source == delivered.path
                        || spec.target == delivered.path
                });
            if !affected {
                continue;
            }

            if delivered.metadata.state == STATE_DELETED
                && link.metadata.state != STATE_DELETED
                && decoded.as_ref().is_ok_and(|spec| {
                    spec.relation == delivered.path
                        || spec.source == delivered.path
                        || spec.target == delivered.path
                })
            {
                if scheduled_deletions.insert(link.path.clone()) {
                    operations.push(Mutation::DeleteResource {
                        resource_path: link.path.clone(),
                        expected_revision: link.revision,
                    });
                    operations.push(Mutation::UpdateResourceStatus {
                        resource_path: link.path.clone(),
                        expected_revision: link.revision + 1,
                        status: ResourceStatus {
                            metadata: link.status_metadata(STATE_DELETED),
                            spec: link.spec.clone(),
                        },
                    });
                }
                if let Ok(spec) = decoded {
                    if spec.source == delivered.path {
                        if let Some(relation) = by_path.get(&spec.relation) {
                            if serde_json::from_value::<RelationSpec>(relation.spec.clone())
                                .is_ok_and(|relation| {
                                    relation.on_source_delete == OnSourceDelete::Cascade
                                })
                            {
                                if let Some(target) = by_path.get(&spec.target) {
                                    if target.metadata.state != STATE_DELETED
                                        && scheduled_deletions.insert(target.path.clone())
                                    {
                                        operations.push(Mutation::DeleteResource {
                                            resource_path: target.path.clone(),
                                            expected_revision: target.revision,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                continue;
            }

            let status = Self::status_for(link, &by_path);
            if link.status != status {
                operations.push(Mutation::UpdateResourceStatus {
                    resource_path: link.path.clone(),
                    expected_revision: link.revision,
                    status,
                });
            }
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

fn validate_link(link: &Resource, resources: &BTreeMap<String, Resource>) -> Result<(), String> {
    let spec: LinkSpec =
        serde_json::from_value(link.spec.clone()).map_err(|error| error.to_string())?;
    let relation = resources
        .get(&spec.relation)
        .ok_or_else(|| format!("Relation {} does not exist", spec.relation))?;
    if relation.manifest != RELATION_MANIFEST {
        return Err(format!("{} is not a Relation", relation.path));
    }
    let relation_spec: RelationSpec =
        serde_json::from_value(relation.spec.clone()).map_err(|error| error.to_string())?;
    let source = resources
        .get(&spec.source)
        .ok_or_else(|| format!("Source {} does not exist", spec.source))?;
    let target = resources
        .get(&spec.target)
        .ok_or_else(|| format!("Target {} does not exist", spec.target))?;
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

fn execution(error: impl std::fmt::Display) -> DriverError {
    DriverError::Execution(error.to_string())
}
