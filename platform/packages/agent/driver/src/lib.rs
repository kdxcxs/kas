use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use async_trait::async_trait;
use kas_core::{
    DriverExecution, LinkSpec, Mutation, PlannedResource, PlannedResourceMetadata, RbacRuleSpec,
    Resource, ResourceStatus, RoleBindingSpec, RoleSpec, RunSpec, ServiceAccountSpec,
};
use kas_driver::{Driver, DriverError};
use kas_skill_driver::{
    extract_bundle, SkillSpec, BUNDLE_RELATION, KAS_SKILL_PATH, SKILL_MANIFEST, USES_RELATION,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const AGENT_MANIFEST: &str = "/manifests/agent";
const MESSAGE_MANIFEST: &str = "/manifests/message";
const FILE_MANIFEST: &str = "/manifests/file";
const SESSION_MANIFEST: &str = "/manifests/session";
const APPROVAL_MANIFEST: &str = "/manifests/approval";
const APPROVAL_RESULT_MANIFEST: &str = "/manifests/approval-result";
const MESSAGE_ACTION: &str = "/manifests/agent/actions/message";
const LINK_MANIFEST: &str = "/builtin/link";
const SERVICE_ACCOUNT_MANIFEST: &str = "/builtin/service-account";
const ROLE_MANIFEST: &str = "/builtin/role";
const ROLE_BINDING_MANIFEST: &str = "/builtin/role-binding";
const AUTHORED_BY: &str = "/manifests/message/relations/authored-by";
const REPLIES_TO: &str = "/manifests/message/relations/replies-to";
const MESSAGE_THREAD: &str = "/manifests/message/relations/message-thread";
const THREAD_SESSION: &str = "/manifests/session/relations/thread-session";
const AGENT_SESSION: &str = "/manifests/session/relations/agent-session";
const SERVICE_ACCOUNT_RELATION: &str = "/manifests/agent/relations/service-account";
const AGENT_RUNTIME_ROLE: &str = "/manifests/agent/roles/runtime";
const ATTACHED_TO: &str = "/manifests/file/relations/attached-to";

#[derive(Debug, Clone)]
pub struct AgentDriver {
    api: String,
    token: String,
    codex: PathBuf,
    codex_home: Option<PathBuf>,
    file_api: String,
    approval_api: String,
    data_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct AgentSpec {
    working_directory: PathBuf,
}

#[derive(Debug, Deserialize)]
struct IssuedCredential {
    token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct SessionSpec {
    provider: String,
    session_id: String,
    cursor: String,
}

#[derive(Debug, Clone, Deserialize)]
struct FileSpec {
    filename: String,
    media_type: String,
    size: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct CodexRun {
    session_id: String,
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
            codex_home: None,
            file_api: "http://127.0.0.1:3001".into(),
            approval_api: "http://127.0.0.1:3003".into(),
            data_dir: None,
        }
    }

    pub fn with_codex_home(mut self, path: impl Into<PathBuf>) -> Self {
        self.codex_home = Some(path.into());
        self
    }

    pub fn with_file_api(mut self, api: impl Into<String>) -> Self {
        self.file_api = api.into().trim_end_matches('/').to_owned();
        self
    }

    pub fn with_approval_api(mut self, api: impl Into<String>) -> Self {
        self.approval_api = api.into().trim_end_matches('/').to_owned();
        self
    }

    pub fn with_data_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(path.into());
        self
    }

    fn list_resources(&self, manifest: &str) -> Result<Vec<Resource>, DriverError> {
        let api = self.api.clone();
        let token = self.token.clone();
        let manifest = manifest.to_owned();
        std::thread::spawn(move || {
            reqwest::blocking::Client::new()
                .get(format!("{api}/resources"))
                .bearer_auth(token)
                .query(&[("manifest", manifest.as_str())])
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::json)
                .map_err(|error| format!("could not list {manifest} Resources: {error}"))
        })
        .join()
        .map_err(|_| execution_error("Resource REST worker panicked"))?
        .map_err(execution_error)
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

    fn thread_messages(
        &self,
        thread_path: &str,
        current_message_path: &str,
        cursor: Option<&str>,
    ) -> Result<String, DriverError> {
        let links = self.list_resources(LINK_MANIFEST)?;
        let files = self.list_resources(FILE_MANIFEST)?;
        let mut messages = self
            .list_resources(MESSAGE_MANIFEST)?
            .into_iter()
            .filter(|message| message.metadata.state != kas_core::STATE_DELETED)
            .filter(|message| {
                links.iter().any(|resource| {
                    serde_json::from_value::<LinkSpec>(resource.spec.clone()).is_ok_and(|link| {
                        link.relation == MESSAGE_THREAD
                            && link.source == message.path
                            && link.target == thread_path
                    })
                })
            })
            .collect::<Vec<_>>();
        messages.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.path.cmp(&right.path))
        });

        let end = messages
            .iter()
            .position(|message| message.path == current_message_path)
            .ok_or_else(|| {
                execution_error(format!(
                    "Message {current_message_path} was not found in Thread {thread_path}"
                ))
            })?;
        let start = cursor
            .and_then(|cursor| messages.iter().position(|message| message.path == cursor))
            .map_or(0, |index| index.saturating_add(1));
        let selected = if start <= end {
            &messages[start..=end]
        } else {
            &messages[end..=end]
        };

        let mut transcript = String::new();
        for message in selected {
            let role = message
                .spec
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let body = message
                .spec
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("");
            let author = links
                .iter()
                .find_map(|resource| {
                    let link = serde_json::from_value::<LinkSpec>(resource.spec.clone()).ok()?;
                    (link.relation == AUTHORED_BY && link.source == message.path)
                        .then_some(link.target)
                })
                .unwrap_or_else(|| "unknown".into());
            transcript.push_str(&format!(
                "[{role} by {author} at {}]\n{body}\n\n",
                message.path
            ));
            let attachments = links
                .iter()
                .filter_map(|resource| {
                    let link = serde_json::from_value::<LinkSpec>(resource.spec.clone()).ok()?;
                    (link.relation == ATTACHED_TO && link.target == message.path)
                        .then_some(link.source)
                })
                .filter_map(|file_path| {
                    let file = files.iter().find(|file| file.path == file_path)?;
                    let spec = serde_json::from_value::<FileSpec>(file.spec.clone()).ok()?;
                    Some((file.path.as_str(), spec))
                })
                .collect::<Vec<_>>();
            if !attachments.is_empty() {
                transcript.push_str("Attachments:\n");
                for (path, spec) in attachments {
                    transcript.push_str(&format!(
                        "- resource: {path}\n  filename: {}\n  media_type: {}\n  size: {} bytes\n  download: curl -sS -G -H \"Authorization: Bearer $KAS_TOKEN\" --data-urlencode \"path={path}\" \"$KAS_FILE_API/files/content\" -o <output-path>\n",
                        spec.filename, spec.media_type, spec.size
                    ));
                }
                transcript.push('\n');
            }
        }
        Ok(transcript.trim_end().to_owned())
    }

    fn prepare_skills(&self, agent_path: &str) -> Result<(PathBuf, Vec<String>), DriverError> {
        let data_dir = self
            .data_dir
            .as_ref()
            .ok_or_else(|| execution_error("KAS_DATA_DIR is required for Agent Skill isolation"))?;
        let agent_key = agent_path.trim_matches('/').replace(['/', '\\'], "_");
        let agent_home = data_dir
            .join("agent-driver")
            .join("agents")
            .join(agent_key)
            .join("codex-home");
        fs::create_dir_all(&agent_home).map_err(|error| {
            execution_error(format!(
                "could not create Agent Codex home {}: {error}",
                agent_home.display()
            ))
        })?;
        if let Some(base_home) = &self.codex_home {
            for name in ["auth.json", "config.toml"] {
                let source = base_home.join(name);
                if source.exists() {
                    fs::copy(&source, agent_home.join(name)).map_err(|error| {
                        execution_error(format!(
                            "could not copy {} into Agent Codex home: {error}",
                            source.display()
                        ))
                    })?;
                }
            }
        }

        let links = self.list_resources(LINK_MANIFEST)?;
        let mut assignments = links
            .iter()
            .filter_map(|resource| {
                let link = serde_json::from_value::<LinkSpec>(resource.spec.clone()).ok()?;
                (link.relation == USES_RELATION && link.source == agent_path)
                    .then_some((link.target, link.metadata))
            })
            .collect::<Vec<_>>();
        assignments.sort_by(|left, right| left.0.cmp(&right.0));

        let staging = tempfile::Builder::new()
            .prefix(".skills-next-")
            .tempdir_in(&agent_home)
            .map_err(|error| execution_error(format!("could not stage Agent Skills: {error}")))?;
        let mut names = BTreeSet::new();
        let mut always = Vec::new();
        for (skill_path, assignment_metadata) in assignments {
            let skill = self
                .fetch_resource(&skill_path)?
                .ok_or_else(|| execution_error(format!("Skill {skill_path} does not exist")))?;
            if skill.manifest != SKILL_MANIFEST
                || skill.metadata.state == kas_core::STATE_DELETED
                || skill.status.metadata.state != kas_core::STATE_AVAILABLE
            {
                return Err(execution_error(format!(
                    "Skill {skill_path} is not available"
                )));
            }
            let spec: SkillSpec = serde_json::from_value(skill.spec.clone())
                .map_err(|error| execution_error(format!("invalid Skill {skill_path}: {error}")))?;
            if !names.insert(spec.name.clone()) {
                return Err(execution_error(format!(
                    "Agent {agent_path} has more than one Skill named {:?}",
                    spec.name
                )));
            }
            let bundle_link = links
                .iter()
                .find_map(|resource| {
                    let link = serde_json::from_value::<LinkSpec>(resource.spec.clone()).ok()?;
                    (link.relation == BUNDLE_RELATION && link.source == skill_path).then_some(link)
                })
                .ok_or_else(|| execution_error(format!("Skill {skill_path} has no bundle Link")))?;
            let bytes = reqwest::blocking::Client::new()
                .get(format!("{}/files/content", self.file_api))
                .bearer_auth(&self.token)
                .query(&[("path", bundle_link.target.as_str())])
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::bytes)
                .map_err(|error| {
                    execution_error(format!(
                        "could not download bundle for Skill {skill_path}: {error}"
                    ))
                })?;
            let extracted = extract_bundle(&bytes, &staging.path().join(&spec.name))
                .map_err(|error| execution_error(format!("invalid Skill {skill_path}: {error}")))?;
            if extracted.spec != spec {
                return Err(execution_error(format!(
                    "Skill {skill_path} spec does not match its bundle"
                )));
            }
            if assignment_metadata.get("mode").and_then(Value::as_str) == Some("always") {
                always.push(spec.name);
            }
        }
        let staged_path = staging.keep();
        let skills_path = agent_home.join("skills");
        if skills_path.exists() {
            fs::remove_dir_all(&skills_path).map_err(|error| {
                execution_error(format!(
                    "could not replace Agent Skills {}: {error}",
                    skills_path.display()
                ))
            })?;
        }
        fs::rename(&staged_path, &skills_path).map_err(|error| {
            execution_error(format!(
                "could not activate Agent Skills {}: {error}",
                skills_path.display()
            ))
        })?;
        Ok((agent_home, always))
    }

    fn run_codex(
        &self,
        agent_path: &str,
        service_account_path: &str,
        thread_path: &str,
        current_message_path: &str,
        reply_path: &str,
        credential_token: &str,
        working_directory: &Path,
        thread_messages: &str,
        session_id: Option<&str>,
    ) -> Result<CodexRun, DriverError> {
        if !working_directory.is_dir() {
            return Err(execution_error(format!(
                "working directory {} does not exist or is not a directory",
                working_directory.display()
            )));
        }
        let (agent_codex_home, always_skills) = self.prepare_skills(agent_path)?;
        let required_skills = always_skills
            .iter()
            .map(|name| format!("${name}"))
            .collect::<Vec<_>>()
            .join(" ");
        let kas_context = kas_bootstrap_context(
            agent_path,
            service_account_path,
            thread_path,
            &required_skills,
        );
        let prompt = if session_id.is_some() {
            format!(
                "KAS Thread update.\n\n{kas_context}\n\nNew messages since your previous turn:\n{thread_messages}\n\nComplete the requested work, then publish your reply to the latest Message through the KAS API as required by $kas. Your terminal assistant response is not forwarded to the Thread."
            )
        } else {
            format!(
                "{kas_context}\n\nThread history through the Message that mentioned you:\n{thread_messages}\n\nComplete the requested work, then publish your reply to the latest Message through the KAS API as required by $kas. Your terminal assistant response is not forwarded to the Thread."
            )
        };
        let mut command = Command::new(&self.codex);
        command
            .arg("--ask-for-approval")
            .arg("never")
            .arg("--sandbox")
            .arg("workspace-write")
            .arg("-c")
            .arg("sandbox_workspace_write.network_access=true")
            .arg("exec");
        if let Some(session_id) = session_id {
            command.arg("resume").arg(session_id);
        } else {
            command.arg("-C").arg(working_directory);
        }
        command
            .arg("--skip-git-repo-check")
            .arg("--json")
            .arg("-")
            .current_dir(working_directory)
            .env_remove("KAS_DRIVER_TOKEN")
            .env_remove("KAS_DRIVER_PATH")
            .env_remove("KAS_DRIVER_GENERATION")
            .env_remove("KAS_MANIFEST_PATH")
            .env_remove("KAS_PACKAGE_ROOT")
            .env_remove("KAS_DATA_DIR")
            .env("KAS_API", &self.api)
            .env("KAS_FILE_API", &self.file_api)
            .env("KAS_APPROVAL_API", &self.approval_api)
            .env("KAS_TOKEN", credential_token)
            .env("KAS_AGENT_PATH", agent_path)
            .env("KAS_SERVICE_ACCOUNT_PATH", service_account_path)
            .env("KAS_THREAD_PATH", thread_path)
            .env("KAS_MESSAGE_PATH", current_message_path)
            .env("KAS_REPLY_PATH", reply_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.env("CODEX_HOME", agent_codex_home);
        let mut child = command.spawn().map_err(|error| {
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
        let emitted_session_id = emitted_session_id(&output.stdout);
        let session_id = match (session_id, emitted_session_id) {
            (Some(expected), Some(actual)) if actual != expected => {
                return Err(execution_error(format!(
                    "Codex resumed unexpected Session {actual}; expected {expected}"
                )));
            }
            (Some(expected), _) => expected.to_owned(),
            (None, Some(created)) => created,
            (None, None) => {
                return Err(execution_error(
                    "Codex JSON output did not include thread.started.thread_id",
                ));
            }
        };
        Ok(CodexRun { session_id })
    }

    fn validate_agent_reply(
        &self,
        reply_path: &str,
        agent_path: &str,
        message_path: &str,
        thread_path: &str,
    ) -> Result<(), DriverError> {
        let reply = self
            .fetch_resource(reply_path)?
            .ok_or_else(|| execution_error(format!("Agent did not create reply {reply_path}")))?;
        if reply.manifest != MESSAGE_MANIFEST
            || reply.spec.get("role").and_then(Value::as_str) != Some("assistant")
            || reply.spec.get("body").and_then(Value::as_str).is_none()
        {
            return Err(execution_error(format!(
                "Agent reply {reply_path} must be an assistant Message with a body"
            )));
        }
        let links = self.fetch_links(reply_path)?;
        for (relation, target) in [
            (AUTHORED_BY, agent_path),
            (REPLIES_TO, message_path),
            (MESSAGE_THREAD, thread_path),
        ] {
            let exists = links.iter().any(|resource| {
                serde_json::from_value::<LinkSpec>(resource.spec.clone()).is_ok_and(|link| {
                    link.relation == relation && link.source == reply_path && link.target == target
                })
            });
            if !exists {
                return Err(execution_error(format!(
                    "Agent reply {reply_path} is missing Relation {relation} to {target}"
                )));
            }
        }
        Ok(())
    }

    fn identity_mutations(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let service_account_path = format!("{}/service-account", resource.path);
        let role_binding_path = format!("{}/role-binding", resource.path);
        let skill_role_path = format!("{}/skill-role", resource.path);
        let skill_role_binding_path = format!("{}/skill-role-binding", resource.path);
        let link_path = format!("{}/links/service-account", resource.path);
        let kas_skill_link_path = format!("{}/links/skills/kas", resource.path);
        let mut mutations = Vec::new();
        for path in [
            &kas_skill_link_path,
            &link_path,
            &skill_role_binding_path,
            &skill_role_path,
            &role_binding_path,
            &service_account_path,
        ] {
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
        if self.fetch_resource(&skill_role_path)?.is_none() {
            mutations.push(Mutation::CreateResource {
                resource: planned(
                    skill_role_path.clone(),
                    ROLE_MANIFEST,
                    format!("{}-agent-skills", resource_name(&resource.path)),
                    serde_json::to_value(RoleSpec {
                        description: format!(
                            "Allow {} to manage its own Skills and assignments.",
                            resource.path
                        ),
                        rules: vec![
                            RbacRuleSpec {
                                manifests: vec![SKILL_MANIFEST.into()],
                                verbs: vec!["get".into(), "list".into()],
                                paths: vec![
                                    "/skills/**".into(),
                                    "/users/**/skills/**".into(),
                                    "/agents/**/skills/**".into(),
                                ],
                            },
                            RbacRuleSpec {
                                manifests: vec![SKILL_MANIFEST.into()],
                                verbs: vec!["create".into(), "update".into(), "delete".into()],
                                paths: vec![format!("{}/skills/**", resource.path)],
                            },
                            RbacRuleSpec {
                                manifests: vec![FILE_MANIFEST.into()],
                                verbs: vec!["upload".into(), "download".into()],
                                paths: vec![format!("/files{}/skills/**", resource.path)],
                            },
                            RbacRuleSpec {
                                manifests: vec![LINK_MANIFEST.into()],
                                verbs: vec![
                                    "get".into(),
                                    "list".into(),
                                    "create".into(),
                                    "update".into(),
                                    "delete".into(),
                                ],
                                paths: vec![
                                    format!("{}/links/skills/**", resource.path),
                                    format!("{}/skills/**", resource.path),
                                ],
                            },
                            RbacRuleSpec {
                                manifests: vec![
                                    APPROVAL_MANIFEST.into(),
                                    APPROVAL_RESULT_MANIFEST.into(),
                                ],
                                verbs: vec!["get".into(), "list".into()],
                                paths: vec![format!("/approvals{}/**", resource.path)],
                            },
                            RbacRuleSpec {
                                manifests: vec![LINK_MANIFEST.into()],
                                verbs: vec!["get".into(), "list".into()],
                                paths: vec![format!("/approvals{}/**", resource.path)],
                            },
                        ],
                        system_role: None,
                    })
                    .map_err(|error| execution_error(error.to_string()))?,
                ),
            });
        }
        if self.fetch_resource(&skill_role_binding_path)?.is_none() {
            mutations.push(Mutation::CreateResource {
                resource: planned(
                    skill_role_binding_path,
                    ROLE_BINDING_MANIFEST,
                    format!("{}-agent-skills", resource_name(&resource.path)),
                    serde_json::to_value(RoleBindingSpec {
                        role: skill_role_path,
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
        if self.fetch_resource(KAS_SKILL_PATH)?.is_none() {
            return Err(execution_error(format!(
                "required platform Skill {KAS_SKILL_PATH} is not installed"
            )));
        }
        if self.fetch_resource(&kas_skill_link_path)?.is_none() {
            mutations.push(Mutation::CreateResource {
                resource: link_resource_with_metadata(
                    kas_skill_link_path,
                    USES_RELATION,
                    resource.path.clone(),
                    KAS_SKILL_PATH,
                    json!({"mode": "always"}),
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
        if resource.manifest == SESSION_MANIFEST {
            serde_json::from_value::<SessionSpec>(resource.spec.clone()).map_err(|error| {
                execution_error(format!(
                    "invalid Session spec at {}: {error}",
                    resource.path
                ))
            })?;
            return Ok(vec![Mutation::UpdateResourceStatus {
                resource_path: resource.path.clone(),
                expected_revision: resource.revision,
                status: ResourceStatus {
                    metadata: resource.status_metadata(resource.metadata.state.clone()),
                    spec: resource.spec.clone(),
                },
            }]);
        }
        if resource.manifest != AGENT_MANIFEST {
            return Err(execution_error(format!(
                "Agent Driver cannot reconcile Manifest {}",
                resource.manifest
            )));
        }
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
        let driver = self.clone();
        let resource = resource.clone();
        let action = action.clone();
        let run = run.clone();
        tokio::task::spawn_blocking(move || {
            let self_ = &driver;
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
            let message = self_
                .fetch_resource(message_path)?
                .ok_or_else(|| execution_error(format!("Message {message_path} does not exist")))?;
            if message.spec.get("body").and_then(Value::as_str).is_none() {
                return Err(execution_error(format!(
                    "Message {message_path} has no spec.body"
                )));
            }
            let belongs_to_thread = self_.fetch_links(message_path)?.into_iter().any(|link| {
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
            let session_path = session_path(thread_path, &resource.path);
            let session = self_.fetch_resource(&session_path)?;
            if session
                .as_ref()
                .is_some_and(|session| session.metadata.state == kas_core::STATE_DELETED)
            {
                return Err(execution_error(format!(
                    "Session {session_path} is still being reset"
                )));
            }
            let session_spec = session
                .as_ref()
                .map(|session| {
                    serde_json::from_value::<SessionSpec>(session.spec.clone()).map_err(|error| {
                        execution_error(format!("invalid Session spec at {session_path}: {error}"))
                    })
                })
                .transpose()?;
            if session_spec
                .as_ref()
                .is_some_and(|session| session.provider != "codex")
            {
                return Err(execution_error(format!(
                    "Session {session_path} is not a Codex Session"
                )));
            }
            let thread_messages = self_.thread_messages(
                thread_path,
                message_path,
                session_spec.as_ref().map(|session| session.cursor.as_str()),
            )?;
            let service_account_path = format!("{}/service-account", resource.path);
            let credential = self_.issue_agent_credential(&service_account_path)?;
            let reply_path = format!("/messages/{}/assistant", run_spec.request_id);
            let codex_run = self_.run_codex(
                &resource.path,
                &service_account_path,
                thread_path,
                message_path,
                &reply_path,
                &credential.token,
                &spec.working_directory,
                &thread_messages,
                session_spec
                    .as_ref()
                    .map(|session| session.session_id.as_str()),
            )?;
            self_.validate_agent_reply(&reply_path, &resource.path, message_path, thread_path)?;
            let next_session_spec = serde_json::to_value(SessionSpec {
                provider: "codex".into(),
                session_id: codex_run.session_id,
                cursor: reply_path.clone(),
            })
            .map_err(|error| execution_error(error.to_string()))?;
            let mutations = if let Some(session) = session {
                vec![Mutation::UpdateResource {
                    resource_path: session.path.clone(),
                    expected_revision: session.revision,
                    metadata: None,
                    spec: next_session_spec,
                }]
            } else {
                vec![
                    Mutation::CreateResource {
                        resource: planned(
                            session_path.clone(),
                            SESSION_MANIFEST,
                            format!(
                                "{}-{}",
                                resource_name(thread_path),
                                resource_name(&resource.path)
                            ),
                            next_session_spec,
                        ),
                    },
                    Mutation::CreateResource {
                        resource: link_resource(
                            format!("{session_path}/links/thread"),
                            THREAD_SESSION,
                            thread_path,
                            session_path.clone(),
                        )?,
                    },
                    Mutation::CreateResource {
                        resource: link_resource(
                            format!("{session_path}/links/agent"),
                            AGENT_SESSION,
                            resource.path.clone(),
                            session_path.clone(),
                        )?,
                    },
                ]
            };
            Ok(DriverExecution {
                output: json!({ "reply_message_path": reply_path }),
                mutations,
            })
        })
        .await
        .map_err(|error| execution_error(format!("Agent execution worker failed: {error}")))?
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
    link_resource_with_metadata(path, relation, source, target, json!({}))
}

fn link_resource_with_metadata(
    path: impl Into<String>,
    relation: impl Into<String>,
    source: impl Into<String>,
    target: impl Into<String>,
    metadata: Value,
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
            metadata,
        })
        .map_err(|error| execution_error(error.to_string()))?,
    ))
}

fn resource_name(path: &str) -> &str {
    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("resource")
}

fn session_path(thread_path: &str, agent_path: &str) -> String {
    format!("{thread_path}/sessions/{}", path_slug(agent_path))
}

fn path_slug(path: &str) -> String {
    let mut slug = String::new();
    let mut separated = false;
    for character in path.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            separated = false;
        } else if !slug.is_empty() && !separated {
            slug.push('-');
            separated = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

fn kas_bootstrap_context(
    agent_path: &str,
    service_account_path: &str,
    thread_path: &str,
    required_skills: &str,
) -> String {
    format!(
        "You are running as KAS Agent {agent_path}. KAS is a resource-oriented collaboration \
platform; platform objects and their relationships are represented by Resources and Links. \
Your KAS identity is ServiceAccount {service_account_path}. The current KAS Thread is \
{thread_path}. KAS_API, KAS_FILE_API, KAS_APPROVAL_API, KAS_TOKEN, KAS_AGENT_PATH, \
KAS_SERVICE_ACCOUNT_PATH, KAS_THREAD_PATH, KAS_MESSAGE_PATH, and KAS_REPLY_PATH are available \
in the environment. KAS_TOKEN is your scoped credential and must \
not be disclosed. Required Skills for this run: \
{required_skills}. Read and follow each required Skill before acting; use $kas for the full KAS \
protocol and operating instructions."
    )
}

fn emitted_session_id(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|event| {
            (event.get("type").and_then(Value::as_str) == Some("thread.started"))
                .then(|| {
                    event
                        .get("thread_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .flatten()
        })
}

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}

#[cfg(test)]
mod tests {
    use super::{emitted_session_id, kas_bootstrap_context, session_path};

    #[test]
    fn session_path_is_stable_for_a_thread_agent_pair() {
        assert_eq!(
            session_path("/threads/planning", "/agents/Release Planner"),
            "/threads/planning/sessions/agents-release-planner"
        );
    }

    #[test]
    fn reads_codex_thread_started_event() {
        let output = br#"{"type":"turn.started"}
{"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}
{"type":"turn.completed"}"#;
        assert_eq!(
            emitted_session_id(output).as_deref(),
            Some("0199a213-81c0-7800-8aa1-bbab2a035a53")
        );
    }

    #[test]
    fn bootstrap_context_identifies_the_kas_environment_before_skill_instructions() {
        let context = kas_bootstrap_context(
            "/agents/reviewer",
            "/agents/reviewer/service-account",
            "/threads/review",
            "$kas",
        );
        assert!(context.contains("running as KAS Agent /agents/reviewer"));
        assert!(context.contains("ServiceAccount /agents/reviewer/service-account"));
        assert!(context.contains("current KAS Thread is /threads/review"));
        assert!(context.contains("KAS_APPROVAL_API"));
        assert!(context.contains("KAS_REPLY_PATH"));
        assert!(context.contains("use $kas for the full KAS protocol"));
    }
}
