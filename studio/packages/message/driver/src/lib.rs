use async_trait::async_trait;
use kas_core::{run_path, CreateRun, LinkSpec, Mutation, Resource, ResourceStatus};
use kas_driver::{Driver, DriverError};
use reqwest::StatusCode;
use serde_json::json;
use uuid::Uuid;

const MESSAGE_MANIFEST: &str = "/packages/studio/message/manifest";
const THREAD_MANIFEST: &str = "/packages/studio/thread/manifest";
const AGENT_MANIFEST: &str = "/packages/studio/agent/manifest";
const LINK_MANIFEST: &str = "/packages/kas/link/manifest";
const DRIVER_SUBJECT: &str = "/packages/studio/message/service-accounts/driver";
const MESSAGE_ACTION: &str = "/packages/studio/agent/actions/message";
const MESSAGE_THREAD: &str = "/packages/studio/message/relations/message-thread";
const MENTIONED: &str = "/packages/studio/message/relations/mentioned";
const ATTACHED_TO: &str = "/packages/studio/file/relations/attached-to";
const PARTICIPANTS: &str = "/packages/studio/thread/relations/participants";

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

    fn create_run(&self, input: CreateRun) -> Result<Resource, DriverError> {
        let api = self.api.clone();
        let token = self.token.clone();
        std::thread::spawn(move || {
            reqwest::blocking::Client::new()
                .post(format!("{api}/runs"))
                .bearer_auth(token)
                .json(&input)
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::json)
                .map_err(|error| format!("could not create Agent Run: {error}"))
        })
        .join()
        .map_err(|_| execution_error("Run REST worker panicked"))?
        .map_err(execution_error)
    }

    fn reconcile_message(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let body = resource
            .spec
            .get("body")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if body.trim().is_empty()
            && !self.list_links()?.iter().any(|link_resource| {
                serde_json::from_value::<LinkSpec>(link_resource.spec.clone()).is_ok_and(|link| {
                    link.relation == ATTACHED_TO
                        && link.target == resource.path
                        && link_resource.metadata.state != kas_core::STATE_DELETED
                })
            })
        {
            return Err(execution_error(format!(
                "Message {} requires text or an attached File",
                resource.path
            )));
        }
        Ok(vec![Mutation::UpdateResourceStatus {
            resource_path: resource.path.clone(),
            expected_revision: resource.revision,
            status: ResourceStatus {
                metadata: resource.status_metadata(resource.metadata.state.clone()),
                spec: resource.spec.clone(),
            },
        }])
    }

    fn fanout_ready_mentions(&self) -> Result<Vec<Mutation>, DriverError> {
        let links = self.list_links()?;
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
            let request_id = Uuid::new_v5(&Uuid::NAMESPACE_URL, mention_resource.path.as_bytes());
            let expected_run = run_path(DRIVER_SUBJECT, MESSAGE_ACTION, request_id)
                .map_err(|error| execution_error(error.to_string()))?;
            if self.fetch_resource(&expected_run)?.is_some() {
                continue;
            }
            self.create_run(CreateRun {
                request_id,
                resource: agent.path,
                action: MESSAGE_ACTION.into(),
                input: json!({
                    "message_path": message.path,
                    "thread_path": thread.path
                }),
            })?;
        }
        Ok(Vec::new())
    }
}

#[async_trait]
impl Driver for MessageDriver {
    fn name(&self) -> &str {
        "message-fanout"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        if resource.manifest == MESSAGE_MANIFEST {
            return self.reconcile_message(resource);
        }
        if resource.manifest == LINK_MANIFEST {
            if resource.metadata.state == kas_core::STATE_DELETED {
                return Ok(Vec::new());
            }
            let Ok(link) = serde_json::from_value::<LinkSpec>(resource.spec.clone()) else {
                return Ok(Vec::new());
            };
            if link.relation == ATTACHED_TO {
                let Some(message) = self.fetch_resource(&link.target)? else {
                    return Ok(Vec::new());
                };
                if message.manifest == MESSAGE_MANIFEST {
                    return self.reconcile_message(&message);
                }
            }
            if [MENTIONED, MESSAGE_THREAD, PARTICIPANTS].contains(&link.relation.as_str()) {
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

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}

#[cfg(test)]
mod tests {
    use kas_core::run_path;
    use uuid::Uuid;

    use super::{DRIVER_SUBJECT, MESSAGE_ACTION};

    #[test]
    fn mention_run_path_is_stable() {
        let path = "/packages/studio/message/messages/one/links/mentioned/reviewer";
        let request_id = Uuid::new_v5(&Uuid::NAMESPACE_URL, path.as_bytes());
        assert_eq!(
            run_path(DRIVER_SUBJECT, MESSAGE_ACTION, request_id).unwrap(),
            run_path(DRIVER_SUBJECT, MESSAGE_ACTION, request_id).unwrap()
        );
    }
}
