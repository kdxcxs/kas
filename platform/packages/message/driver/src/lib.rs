use async_trait::async_trait;
use kas_core::{
    LinkSpec, Mutation, PlannedResource, PlannedResourceMetadata, Resource, ResourceStatus,
};
use kas_driver::{Driver, DriverError};
use reqwest::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

const MESSAGE_MANIFEST: &str = "/manifests/message";
const THREAD_MANIFEST: &str = "/manifests/thread";
const AGENT_MANIFEST: &str = "/manifests/agent";
const LINK_MANIFEST: &str = "/builtin/link";
const RUN_MANIFEST: &str = "/builtin/run";
const MESSAGE_ACTION: &str = "/manifests/agent/actions/message";
const MESSAGE_THREAD: &str = "/manifests/message/relations/message-thread";
const MENTIONED: &str = "/manifests/message/relations/mentioned";
const ATTACHED_TO: &str = "/manifests/file/relations/attached-to";
const PARTICIPANTS: &str = "/manifests/thread/relations/participants";

#[derive(Debug, Clone)]
pub struct MessageDriver {
    api: String,
    token: String,
}

impl MessageDriver {
    pub fn new(api: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            api: api.into().trim_end_matches('/').to_owned(),
            token: token.into(),
        }
    }

    fn fetch_resource(&self, path: &str) -> Result<Option<Resource>, DriverError> {
        let api = self.api.clone();
        let token = self.token.clone();
        let path = path.to_owned();
        std::thread::spawn(move || {
            let response = reqwest::blocking::Client::new()
                .get(format!("{api}/resources/by-path"))
                .bearer_auth(token)
                .query(&[("path", path.as_str())])
                .send()
                .map_err(|error| format!("could not load Resource {path}: {error}"))?;
            if response.status() == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            response
                .error_for_status()
                .and_then(reqwest::blocking::Response::json)
                .map(Some)
                .map_err(|error| format!("could not load Resource {path}: {error}"))
        })
        .join()
        .map_err(|_| execution_error("Resource REST worker panicked"))?
        .map_err(execution_error)
    }

    fn list_links(&self) -> Result<Vec<Resource>, DriverError> {
        let api = self.api.clone();
        let token = self.token.clone();
        std::thread::spawn(move || {
            reqwest::blocking::Client::new()
                .get(format!("{api}/resources"))
                .bearer_auth(token)
                .query(&[("manifest", LINK_MANIFEST)])
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::json)
                .map_err(|error| format!("could not list Links: {error}"))
        })
        .join()
        .map_err(|_| execution_error("Link REST worker panicked"))?
        .map_err(execution_error)
    }

    fn link_target(links: &[Resource], relation: &str, source: &str) -> Option<String> {
        links.iter().find_map(|resource| {
            let link: LinkSpec = serde_json::from_value(resource.spec.clone()).ok()?;
            (link.relation == relation && link.source == source).then_some(link.target)
        })
    }

    fn has_link(links: &[Resource], relation: &str, source: &str, target: &str) -> bool {
        links.iter().any(|resource| {
            serde_json::from_value::<LinkSpec>(resource.spec.clone()).is_ok_and(|link| {
                link.relation == relation && link.source == source && link.target == target
            })
        })
    }

    fn fanout_ready_mentions(&self) -> Result<Vec<Mutation>, DriverError> {
        let links = self.list_links()?;
        let mut mutations = Vec::new();
        for mention_resource in links
            .iter()
            .filter(|resource| resource.metadata.state != kas_core::STATE_DELETED)
        {
            let Ok(mention) = serde_json::from_value::<LinkSpec>(mention_resource.spec.clone())
            else {
                continue;
            };
            if mention.relation != MENTIONED {
                continue;
            }
            let Some(message) = self.fetch_resource(&mention.source)? else {
                continue;
            };
            let Some(agent) = self.fetch_resource(&mention.target)? else {
                continue;
            };
            if message.manifest != MESSAGE_MANIFEST || agent.manifest != AGENT_MANIFEST {
                continue;
            }
            let Some(thread_path) = Self::link_target(&links, MESSAGE_THREAD, &message.path) else {
                continue;
            };
            let Some(thread) = self.fetch_resource(&thread_path)? else {
                continue;
            };
            if thread.manifest != THREAD_MANIFEST
                || !Self::has_link(&links, PARTICIPANTS, &thread.path, &agent.path)
            {
                continue;
            }
            let run_path = format!("{}/run", mention_resource.path);
            if self.fetch_resource(&run_path)?.is_some() {
                continue;
            }
            let request_id = Uuid::new_v4();
            mutations.push(Mutation::CreateResource {
                resource: planned(
                    run_path,
                    RUN_MANIFEST,
                    request_id.to_string(),
                    json!({
                        "request_id": request_id,
                        "resource": agent.path,
                        "action": MESSAGE_ACTION,
                        "input": {
                            "message_path": message.path,
                            "thread_path": thread.path
                        }
                    }),
                ),
            });
        }
        Ok(mutations)
    }
}

#[async_trait]
impl Driver for MessageDriver {
    fn name(&self) -> &str {
        "message-fanout"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        if resource.manifest == MESSAGE_MANIFEST {
            let body = resource
                .spec
                .get("body")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if body.trim().is_empty()
                && !self.list_links()?.iter().any(|link_resource| {
                    serde_json::from_value::<LinkSpec>(link_resource.spec.clone()).is_ok_and(
                        |link| {
                            link.relation == ATTACHED_TO
                                && link.target == resource.path
                                && link_resource.metadata.state != kas_core::STATE_DELETED
                        },
                    )
                })
            {
                return Err(execution_error(format!(
                    "Message {} requires text or an attached File",
                    resource.path
                )));
            }
            return Ok(vec![Mutation::UpdateResourceStatus {
                resource_path: resource.path.clone(),
                expected_revision: resource.revision,
                status: ResourceStatus {
                    metadata: resource.status_metadata(resource.metadata.state.clone()),
                    spec: resource.spec.clone(),
                },
            }]);
        }
        if resource.manifest == LINK_MANIFEST {
            let relation = serde_json::from_value::<LinkSpec>(resource.spec.clone())
                .map(|link| link.relation)
                .unwrap_or_default();
            if [MENTIONED, MESSAGE_THREAD, PARTICIPANTS].contains(&relation.as_str()) {
                return self.fanout_ready_mentions();
            }
        }
        Ok(Vec::new())
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

fn planned(
    path: impl Into<String>,
    manifest: impl Into<String>,
    name: impl Into<String>,
    spec: Value,
) -> PlannedResource {
    PlannedResource {
        path: path.into(),
        metadata: PlannedResourceMetadata {
            manifest: manifest.into(),
            name: name.into(),
            state: String::new(),
        },
        spec,
        status: ResourceStatus::default(),
    }
}

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn mention_run_path_is_stable() {
        let path = "/messages/one/links/mentioned/reviewer";
        assert_eq!(
            format!("{path}/run"),
            "/messages/one/links/mentioned/reviewer/run"
        );
    }
}
