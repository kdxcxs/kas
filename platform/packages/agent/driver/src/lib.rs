use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use kas_core::{
    Action, DriverExecution, Mutation, ObjectKind, ObjectRef, PlannedLink, PlannedResource,
    Resource, Run,
};
use kas_driver::{Driver, DriverError};
use serde::Deserialize;
use serde_json::{json, Value};

const MESSAGE_MANIFEST: &str = "/manifests/message";
const MESSAGE_ACTION: &str = "/manifests/agent/actions/message";
const AUTHORED_BY: &str = "/manifests/message/relations/authored-by";
const REPLIES_TO: &str = "/manifests/message/relations/replies-to";
const THREAD_ROOT: &str = "/manifests/message/relations/thread-root";

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

    fn run_codex(
        &self,
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
        let prompt = if instructions.trim().is_empty() {
            format!("User message:\n{message}")
        } else {
            format!(
                "Agent instructions:\n{}\n\nUser message:\n{message}",
                instructions.trim()
            )
        };
        let mut child = Command::new(&self.codex)
            .arg("--ask-for-approval")
            .arg("never")
            .arg("--sandbox")
            .arg("workspace-write")
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

    fn reconcile(&self, resource: &Resource) -> Result<Value, DriverError> {
        let spec: AgentSpec = serde_json::from_value(resource.spec.clone())
            .map_err(|error| execution_error(format!("invalid Agent spec: {error}")))?;
        Ok(json!({
            "ready": spec.working_directory.is_dir(),
            "codex": self.codex,
            "working_directory": spec.working_directory
        }))
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
        let reply = self.run_codex(&spec.working_directory, &spec.instructions, body)?;
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

fn planned_link(
    path: String,
    source: ObjectRef,
    relation_path: &str,
    target: ObjectRef,
) -> PlannedLink {
    PlannedLink {
        path,
        source,
        relation_path: relation_path.into(),
        target,
        metadata: json!({}),
    }
}

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}
