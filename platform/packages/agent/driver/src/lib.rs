use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use async_trait::async_trait;
use kas_core::{
    DriverExecution, LinkSpec, Mutation, PlannedResource, PlannedResourceMetadata, Resource,
    ResourceStatus, RoleBindingSpec, RunSpec, ServiceAccountSpec,
};
use kas_driver::{Driver, DriverError};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

const MESSAGE_MANIFEST: &str = "/manifests/message";
const MESSAGE_ACTION: &str = "/manifests/agent/actions/message";
const LINK_MANIFEST: &str = "/builtin/link";
const SERVICE_ACCOUNT_MANIFEST: &str = "/builtin/service-account";
const ROLE_BINDING_MANIFEST: &str = "/builtin/role-binding";
const AUTHORED_BY: &str = "/manifests/message/relations/authored-by";
const REPLIES_TO: &str = "/manifests/message/relations/replies-to";
const MESSAGE_THREAD: &str = "/manifests/message/relations/message-thread";
const SERVICE_ACCOUNT_RELATION: &str = "/manifests/agent/relations/service-account";
const AGENT_RUNTIME_ROLE: &str = "/manifests/agent/roles/runtime";

#[derive(Debug, Clone)]
pub struct AgentDriver {
    api: String,
    token: String,
    codex: PathBuf,
}

#[derive(Debug, Deserialize)]
struct AgentSpec {
    #[serde(default)]
    instructions: String,
    working_directory: PathBuf,
}

#[derive(Debug, Deserialize)]
struct IssuedCredential {
    token: String,
}

impl AgentDriver {
    pub fn new(
        api: impl Into<String>,
        token: impl Into<String>,
        codex: impl Into<PathBuf>,
    ) -> Self {
        Self {
            api: api.into().trim_end_matches('/').to_owned(),
            token: token.into(),
            codex: codex.into(),
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

    fn fetch_links(&self, resource_path: &str) -> Result<Vec<Resource>, DriverError> {
        let api = self.api.clone();
        let token = self.token.clone();
        let resource_path = resource_path.to_owned();
        std::thread::spawn(move || {
            let resources: Vec<Resource> = reqwest::blocking::Client::new()
                .get(format!("{api}/resources"))
                .bearer_auth(token)
                .query(&[("manifest", LINK_MANIFEST)])
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::json)
                .map_err(|error| format!("could not list Links: {error}"))?;
            Ok::<Vec<Resource>, String>(
                resources
                    .into_iter()
                    .filter(|resource| {
                        serde_json::from_value::<LinkSpec>(resource.spec.clone()).is_ok_and(
                            |link| link.source == resource_path || link.target == resource_path,
                        )
                    })
                    .collect(),
            )
        })
        .join()
        .map_err(|_| execution_error("Link REST worker panicked"))?
        .map_err(execution_error)
    }

    fn issue_agent_credential(
        &self,
        service_account_path: &str,
    ) -> Result<IssuedCredential, DriverError> {
        let api = self.api.clone();
        let token = self.token.clone();
        let service_account_path = service_account_path.to_owned();
        std::thread::spawn(move || {
            reqwest::blocking::Client::new()
                .post(format!("{api}/credentials/issue"))
                .bearer_auth(token)
                .json(&json!({"subject": service_account_path}))
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::json)
                .map_err(|error| {
                    format!(
                        "could not issue credential for ServiceAccount {service_account_path}: {error}"
                    )
                })
        })
        .join()
        .map_err(|_| execution_error("Credential REST worker panicked"))?
        .map_err(execution_error)
    }

    fn run_codex(
        &self,
        agent_path: &str,
        service_account_path: &str,
        credential_token: &str,
        working_directory: &Path,
        instructions: &str,
        message: &str,
    ) -> Result<String, DriverError> {
        if !working_directory.is_dir() {
            return Err(execution_error(format!(
                "working directory {} does not exist or is not a directory",
                working_directory.display()
            )));
        }
        let output_file = tempfile::NamedTempFile::new()
            .map_err(|error| execution_error(format!("could not create output file: {error}")))?;
        let platform_context = format!(
            r#"KAS platform context:
- KAS is the Resource management and reconciliation platform hosting this Agent.
- Every persistent object is a Resource selected by its metadata.manifest path.
- Your Agent Resource path is {agent_path}.
- Your ServiceAccount path is {service_account_path}.
- Use the KAS REST API at $KAS_API with `Authorization: Bearer $KAS_TOKEN`.
- Read your own Resource with:
  curl -sS -G -H "Authorization: Bearer $KAS_TOKEN" --data-urlencode "path=$KAS_AGENT_PATH" "$KAS_API/resources/by-path"
- List Message Resources with:
  curl -sS -G -H "Authorization: Bearer $KAS_TOKEN" --data-urlencode "manifest=/manifests/message" "$KAS_API/resources"
- List Thread Resources with:
  curl -sS -G -H "Authorization: Bearer $KAS_TOKEN" --data-urlencode "manifest=/manifests/thread" "$KAS_API/resources"
- Create a Message with:
  curl -sS -H "Authorization: Bearer $KAS_TOKEN" -H "Content-Type: application/json" \
    -d '{{"metadata":{{"path":"/messages/example","manifest":"/manifests/message","name":"example"}},"spec":{{"role":"system","body":"example"}}}}' \
    "$KAS_API/resources"
- Operations are restricted by the RBAC permissions of your ServiceAccount.
- Never print, persist, or place $KAS_TOKEN in a Resource, Message, Link, or file."#
        );
        let prompt = format!(
            "{platform_context}\n\nAgent instructions:\n{}\n\nUser message:\n{message}",
            instructions.trim()
        );
        let mut child = Command::new(&self.codex)
            .arg("--ask-for-approval")
            .arg("never")
            .arg("--sandbox")
            .arg("workspace-write")
            .arg("-c")
            .arg("sandbox_workspace_write.network_access=true")
            .arg("exec")
            .arg("--ephemeral")
            .arg("--skip-git-repo-check")
            .arg("--color")
            .arg("never")
            .arg("-C")
            .arg(working_directory)
            .arg("--output-last-message")
            .arg(output_file.path())
            .arg("-")
            .env_remove("KAS_DRIVER_TOKEN")
            .env_remove("KAS_DRIVER_PATH")
            .env_remove("KAS_DRIVER_GENERATION")
            .env_remove("KAS_MANIFEST_PATH")
            .env_remove("KAS_PACKAGE_ROOT")
            .env_remove("KAS_DATA_DIR")
            .env("KAS_API", &self.api)
            .env("KAS_TOKEN", credential_token)
            .env("KAS_AGENT_PATH", agent_path)
            .env("KAS_SERVICE_ACCOUNT_PATH", service_account_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                execution_error(format!(
                    "could not start Codex executable {}: {error}",
                    self.codex.display()
                ))
            })?;
        child
            .stdin
            .take()
            .ok_or_else(|| execution_error("Codex stdin was not available"))?
            .write_all(prompt.as_bytes())
            .map_err(|error| execution_error(format!("could not write Codex prompt: {error}")))?;
        let output = child
            .wait_with_output()
            .map_err(|error| execution_error(format!("could not wait for Codex: {error}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(execution_error(format!(
                "Codex exited with {}: {}{}",
                output.status,
                stderr.trim(),
                if stdout.trim().is_empty() {
                    String::new()
                } else {
                    format!("; stdout: {}", stdout.trim())
                }
            )));
        }
        let reply = std::fs::read_to_string(output_file.path())
            .map_err(|error| execution_error(format!("could not read Codex reply: {error}")))?;
        let reply = reply.trim();
        if reply.is_empty() {
            return Err(execution_error("Codex returned an empty reply"));
        }
        Ok(reply.to_owned())
    }

    fn identity_mutations(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let service_account_path = format!("{}/service-account", resource.path);
        let role_binding_path = format!("{}/role-binding", resource.path);
        let link_path = format!("{}/links/service-account", resource.path);
        let mut mutations = Vec::new();
        for path in [&link_path, &role_binding_path, &service_account_path] {
            if let Some(child) = self.fetch_resource(path)? {
                if child.metadata.state != kas_core::STATE_DELETED {
                    mutations.push(Mutation::DeleteResource {
                        resource_path: child.path.clone(),
                        expected_revision: child.revision,
                    });
                }
            }
        }
        if resource.metadata.state == kas_core::STATE_DELETED {
            return Ok(mutations);
        }
        mutations.clear();
        if self.fetch_resource(&service_account_path)?.is_none() {
            mutations.push(Mutation::CreateResource {
                resource: planned(
                    service_account_path.clone(),
                    SERVICE_ACCOUNT_MANIFEST,
                    format!("{}-agent", resource_name(&resource.path)),
                    serde_json::to_value(ServiceAccountSpec::default())
                        .map_err(|error| execution_error(error.to_string()))?,
                ),
            });
        }
        if self.fetch_resource(&role_binding_path)?.is_none() {
            mutations.push(Mutation::CreateResource {
                resource: planned(
                    role_binding_path,
                    ROLE_BINDING_MANIFEST,
                    format!("{}-agent-runtime", resource_name(&resource.path)),
                    serde_json::to_value(RoleBindingSpec {
                        role: AGENT_RUNTIME_ROLE.into(),
                        subjects: vec![service_account_path.clone()],
                    })
                    .map_err(|error| execution_error(error.to_string()))?,
                ),
            });
        }
        if self.fetch_resource(&link_path)?.is_none() {
            mutations.push(Mutation::CreateResource {
                resource: link_resource(
                    link_path,
                    SERVICE_ACCOUNT_RELATION,
                    resource.path.clone(),
                    service_account_path,
                )?,
            });
        }
        Ok(mutations)
    }
}

#[async_trait]
impl Driver for AgentDriver {
    fn name(&self) -> &str {
        "codex-cli"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let spec: AgentSpec = serde_json::from_value(resource.spec.clone())
            .map_err(|error| execution_error(format!("invalid Agent spec: {error}")))?;
        if resource.metadata.state != kas_core::STATE_DELETED && !spec.working_directory.is_dir() {
            return Err(execution_error(format!(
                "working directory {} does not exist or is not a directory",
                spec.working_directory.display()
            )));
        }
        let mut mutations = self.identity_mutations(resource)?;
        mutations.push(Mutation::UpdateResourceStatus {
            resource_path: resource.path.clone(),
            expected_revision: resource.revision,
            status: ResourceStatus {
                metadata: resource.status_metadata(resource.metadata.state.clone()),
                spec: resource.spec.clone(),
            },
        });
        Ok(mutations)
    }

    async fn execute(
        &self,
        resource: &Resource,
        action: &Resource,
        run: &Resource,
    ) -> Result<DriverExecution, DriverError> {
        if action.path != MESSAGE_ACTION {
            return Err(DriverError::UnsupportedAction(action.path.clone()));
        }
        let spec: AgentSpec = serde_json::from_value(resource.spec.clone())
            .map_err(|error| execution_error(format!("invalid Agent spec: {error}")))?;
        let run_spec: RunSpec = serde_json::from_value(run.spec.clone())
            .map_err(|error| execution_error(format!("invalid Run spec: {error}")))?;
        let message_path = run_spec
            .input
            .get("message_path")
            .and_then(Value::as_str)
            .ok_or_else(|| execution_error("message action requires input.message_path"))?;
        let thread_path = run_spec
            .input
            .get("thread_path")
            .and_then(Value::as_str)
            .ok_or_else(|| execution_error("message action requires input.thread_path"))?;
        let message = self
            .fetch_resource(message_path)?
            .ok_or_else(|| execution_error(format!("Message {message_path} does not exist")))?;
        let body = message
            .spec
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| execution_error(format!("Message {message_path} has no spec.body")))?;
        let belongs_to_thread = self.fetch_links(message_path)?.into_iter().any(|link| {
            serde_json::from_value::<LinkSpec>(link.spec).is_ok_and(|spec| {
                spec.relation == MESSAGE_THREAD
                    && spec.source == message_path
                    && spec.target == thread_path
            })
        });
        if !belongs_to_thread {
            return Err(execution_error(format!(
                "Message {message_path} does not belong to Thread {thread_path}"
            )));
        }
        let service_account_path = format!("{}/service-account", resource.path);
        let credential = self.issue_agent_credential(&service_account_path)?;
        let reply = self.run_codex(
            &resource.path,
            &service_account_path,
            &credential.token,
            &spec.working_directory,
            &spec.instructions,
            body,
        )?;
        let reply_path = format!("/messages/{}/assistant", run_spec.request_id);
        let mut mutations = vec![Mutation::CreateResource {
            resource: planned(
                reply_path.clone(),
                MESSAGE_MANIFEST,
                "assistant-reply",
                json!({
                    "role": "assistant",
                    "body": reply
                }),
            ),
        }];
        for (suffix, relation, target) in [
            ("authored-by", AUTHORED_BY, resource.path.as_str()),
            ("replies-to", REPLIES_TO, message_path),
            ("message-thread", MESSAGE_THREAD, thread_path),
        ] {
            mutations.push(Mutation::CreateResource {
                resource: link_resource(
                    format!("{reply_path}/links/{suffix}"),
                    relation,
                    reply_path.clone(),
                    target.to_owned(),
                )?,
            });
        }
        Ok(DriverExecution {
            output: json!({ "reply_message_path": reply_path }),
            mutations,
        })
    }
}

fn planned(
    path: impl Into<String>,
    manifest: impl Into<String>,
    name: impl Into<String>,
    spec: Value,
) -> PlannedResource {
    PlannedResource {
        metadata: PlannedResourceMetadata {
            path: path.into(),
            manifest: manifest.into(),
            name: name.into(),
            state: String::new(),
        },
        spec,
        status: ResourceStatus::default(),
    }
}

fn link_resource(
    path: impl Into<String>,
    relation: impl Into<String>,
    source: impl Into<String>,
    target: impl Into<String>,
) -> Result<PlannedResource, DriverError> {
    let path = path.into();
    Ok(planned(
        path.clone(),
        LINK_MANIFEST,
        resource_name(&path),
        serde_json::to_value(LinkSpec {
            relation: relation.into(),
            source: source.into(),
            target: target.into(),
            metadata: json!({}),
        })
        .map_err(|error| execution_error(error.to_string()))?,
    ))
}

fn resource_name(path: &str) -> &str {
    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("resource")
}

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}
