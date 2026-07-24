use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use kas_core::{
    Action, DriverExecution, Mutation, ObjectKind, ObjectRef, PlannedLink, PlannedResource,
    RbacSubjectDefinition, RbacSubjectKind, ReconcileObject, Resource, Run,
};
use kas_driver::{Driver, DriverError};
use serde::Deserialize;
use serde_json::{json, Value};

const MESSAGE_MANIFEST: &str = "/manifests/message";
const MESSAGE_ACTION: &str = "/manifests/agent/actions/message";
const AUTHORED_BY: &str = "/manifests/message/relations/authored-by";
const REPLIES_TO: &str = "/manifests/message/relations/replies-to";
const THREAD_ROOT: &str = "/manifests/message/relations/thread-root";
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
    path: String,
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

    fn fetch_message(&self, path: &str) -> Result<Value, DriverError> {
        let api = self.api.clone();
        let token = self.token.clone();
        let path = path.to_owned();
        std::thread::spawn(move || {
            reqwest::blocking::Client::new()
                .get(format!("{api}/resources/by-path"))
                .bearer_auth(token)
                .query(&[("path", path.as_str()), ("include", "relations")])
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::json)
                .map_err(|error| format!("could not load Message {path}: {error}"))
        })
        .join()
        .map_err(|_| execution_error("Message REST worker panicked"))?
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
                .post(format!("{api}/service-accounts/credentials"))
                .bearer_auth(token)
                .query(&[("path", service_account_path.as_str())])
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

    fn revoke_agent_credential(&self, credential_path: &str) -> Result<(), DriverError> {
        let api = self.api.clone();
        let token = self.token.clone();
        let credential_path = credential_path.to_owned();
        std::thread::spawn(move || {
            reqwest::blocking::Client::new()
                .delete(format!("{api}/credentials/by-path"))
                .bearer_auth(token)
                .query(&[("path", credential_path.as_str())])
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .map(|_| ())
                .map_err(|error| {
                    format!("could not revoke Agent credential {credential_path}: {error}")
                })
        })
        .join()
        .map_err(|_| execution_error("Credential revoke REST worker panicked"))?
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
- KAS is the resource management and reconciliation platform hosting this Agent.
- Its core objects are Manifest, Resource, Relation, Link, Action, and Run.
- Your Agent Resource path is {agent_path}.
- Your ServiceAccount path is {service_account_path}.
- Use the KAS REST API at $KAS_API with `Authorization: Bearer $KAS_TOKEN`.
- Read your own Resource with:
  curl -sS -H "Authorization: Bearer $KAS_TOKEN" "$KAS_API/resources/by-path?path=$KAS_AGENT_PATH&include=relations"
- List Message Resources with:
  curl -sS -G -H "Authorization: Bearer $KAS_TOKEN" --data-urlencode "manifest=/manifests/message" "$KAS_API/resources"
- Create Resources with JSON POST requests to $KAS_API/resources.
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
}

impl Driver for AgentDriver {
    fn name(&self) -> &str {
        "codex-cli"
    }

    fn reconcile(&self, object: &ReconcileObject) -> Result<Vec<Mutation>, DriverError> {
        match object {
            ReconcileObject::Resource(resource) => {
                let spec: AgentSpec = serde_json::from_value(resource.spec.clone())
                    .map_err(|error| execution_error(format!("invalid Agent spec: {error}")))?;
                if resource.spec.get("state").and_then(Value::as_str) != Some("deleted")
                    && !spec.working_directory.is_dir()
                {
                    return Err(execution_error(format!(
                        "working directory {} does not exist or is not a directory",
                        spec.working_directory.display()
                    )));
                }
                Ok(vec![Mutation::UpdateResourceStatus {
                    resource_path: resource.path.clone(),
                    expected_revision: resource.revision,
                    status: resource.spec.clone(),
                }])
            }
            ReconcileObject::Link(link) => {
                if link.relation_path != SERVICE_ACCOUNT_RELATION {
                    return Err(execution_error(format!(
                        "Agent Driver cannot reconcile Relation {}",
                        link.relation_path
                    )));
                }
                let agent = link.source.as_ref().ok_or_else(|| {
                    execution_error("Agent ServiceAccount Link has no Agent source")
                })?;
                if agent.kind != ObjectKind::Resource {
                    return Err(execution_error(
                        "Agent ServiceAccount Link source is not a Resource",
                    ));
                }
                let service_account_path = format!("{}/service-account", agent.path);
                let role_binding_path = format!("{}/role-binding", agent.path);
                let service_account = ObjectRef {
                    kind: ObjectKind::ServiceAccount,
                    path: service_account_path.clone(),
                };
                let mut mutations = Vec::new();
                if link.target.as_ref() != Some(&service_account) {
                    mutations.push(Mutation::CreateServiceAccount {
                        path: service_account_path.clone(),
                        name: format!("{}-agent", resource_name(&agent.path)),
                    });
                    mutations.push(Mutation::CreateRoleBinding {
                        path: role_binding_path,
                        name: format!("{}-agent-runtime", resource_name(&agent.path)),
                        role_path: AGENT_RUNTIME_ROLE.into(),
                        subjects: vec![RbacSubjectDefinition {
                            kind: RbacSubjectKind::ServiceAccount,
                            path: service_account_path,
                        }],
                    });
                }
                mutations.push(Mutation::UpdateLink {
                    link_path: link.path.clone(),
                    expected_revision: link.revision,
                    source: link.source.clone(),
                    target: Some(service_account),
                    status: link.spec.clone(),
                });
                Ok(mutations)
            }
        }
    }

    fn execute(
        &self,
        resource: &Resource,
        action: &Action,
        run: &Run,
    ) -> Result<DriverExecution, DriverError> {
        if action.path != MESSAGE_ACTION {
            return Err(DriverError::UnsupportedAction(action.path.clone()));
        }
        let spec: AgentSpec = serde_json::from_value(resource.spec.clone())
            .map_err(|error| execution_error(format!("invalid Agent spec: {error}")))?;
        let message_path = run
            .input
            .get("message_path")
            .and_then(Value::as_str)
            .ok_or_else(|| execution_error("message action requires input.message_path"))?;
        let message = self.fetch_message(message_path)?;
        let body = message
            .pointer("/spec/body")
            .and_then(Value::as_str)
            .ok_or_else(|| execution_error(format!("Message {message_path} has no spec.body")))?;
        let thread_root = message
            .get("links")
            .and_then(Value::as_array)
            .and_then(|links| {
                links.iter().find(|link| {
                    link.get("relation_path").and_then(Value::as_str) == Some(THREAD_ROOT)
                        && link.pointer("/source/path").and_then(Value::as_str)
                            == Some(message_path)
                })
            })
            .and_then(|link| link.pointer("/target/path"))
            .and_then(Value::as_str)
            .unwrap_or(message_path);
        let service_account_path = format!("{}/service-account", resource.path);
        let credential = self.issue_agent_credential(&service_account_path)?;
        let reply = self.run_codex(
            &resource.path,
            &service_account_path,
            &credential.token,
            &spec.working_directory,
            &spec.instructions,
            body,
        );
        let revoke = self.revoke_agent_credential(&credential.path);
        let reply = match (reply, revoke) {
            (Ok(reply), Ok(())) => reply,
            (Err(error), _) => return Err(error),
            (Ok(_), Err(error)) => return Err(error),
        };
        let reply_path = format!("/messages/{}/assistant", run.request_id);
        let reply_ref = ObjectRef {
            kind: ObjectKind::Resource,
            path: reply_path.clone(),
        };
        let links = vec![
            planned_link(
                format!("{reply_path}/links/authored-by"),
                reply_ref.clone(),
                AUTHORED_BY,
                ObjectRef {
                    kind: ObjectKind::Resource,
                    path: resource.path.clone(),
                },
            ),
            planned_link(
                format!("{reply_path}/links/replies-to"),
                reply_ref.clone(),
                REPLIES_TO,
                ObjectRef {
                    kind: ObjectKind::Resource,
                    path: message_path.to_owned(),
                },
            ),
            planned_link(
                format!("{reply_path}/links/thread-root"),
                reply_ref,
                THREAD_ROOT,
                ObjectRef {
                    kind: ObjectKind::Resource,
                    path: thread_root.to_owned(),
                },
            ),
        ];
        Ok(DriverExecution {
            output: json!({ "reply_message_path": reply_path }),
            mutations: vec![Mutation::CreateResource {
                resource: PlannedResource {
                    path: reply_path,
                    manifest: MESSAGE_MANIFEST.into(),
                    name: "assistant-reply".into(),
                    spec: json!({
                        "role": "assistant",
                        "body": reply
                    }),
                    links,
                },
            }],
        })
    }
}

fn resource_name(path: &str) -> &str {
    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("agent")
}

fn planned_link(
    path: String,
    source: ObjectRef,
    relation_path: &str,
    target: ObjectRef,
) -> PlannedLink {
    PlannedLink {
        path,
        source: Some(source),
        relation_path: relation_path.into(),
        target: Some(target),
        spec: json!({ "state": "available" }),
        status: json!({ "state": "available" }),
        metadata: json!({}),
    }
}

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}
