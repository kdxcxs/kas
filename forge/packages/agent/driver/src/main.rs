use std::{
    env,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use async_trait::async_trait;
use kas_core::{
    DriverExecution, LinkSpec, Mutation, PlannedResource, PlannedResourceMetadata, Resource,
    ResourceStatus, RunSpec, ServiceAccountSpec,
};
use kas_driver::{Driver, DriverError, DriverRuntime};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

const AGENT_MANIFEST: &str = "/packages/forge/agent/manifest";
const AGENT_ACTION: &str = "/packages/forge/agent/actions/run";
const SERVICE_ACCOUNT_MANIFEST: &str = "/packages/kas/service-account/manifest";
const LINK_MANIFEST: &str = "/packages/kas/link/manifest";
const ROLE_BINDING_RELATION: &str = "/packages/kas/link/relations/role-binding";
const SERVICE_ACCOUNT_RELATION: &str = "/packages/forge/agent/relations/service-account";
const RUNTIME_ROLE: &str = "/packages/forge/agent/roles/runtime";
const AGENT_ROOT: &str = "/packages/forge/agent";

#[derive(Debug, Clone)]
struct ForgeAgentDriver {
    api: String,
    token: String,
    codex: PathBuf,
    codex_home: Option<PathBuf>,
    package_request_api: String,
}

#[derive(Debug, Deserialize)]
struct AgentSpec {
    working_directory: PathBuf,
    #[allow(dead_code)]
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
struct IssuedCredential {
    token: String,
}

impl ForgeAgentDriver {
    fn reconcile_blocking(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        if resource.manifest != AGENT_MANIFEST {
            return Err(execution_error(format!(
                "Agent Driver cannot reconcile {}",
                resource.manifest
            )));
        }
        let spec: AgentSpec = serde_json::from_value(resource.spec.clone())
            .map_err(|error| execution_error(format!("invalid Agent spec: {error}")))?;
        if resource.metadata.state != kas_core::STATE_DELETED && !spec.working_directory.is_dir() {
            return Err(execution_error(format!(
                "working directory {} does not exist",
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

    fn resource(&self, path: &str) -> Result<Option<Resource>, DriverError> {
        let response = reqwest::blocking::Client::new()
            .get(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.token)
            .query(&[("path", path)])
            .send()
            .map_err(|error| execution_error(format!("could not load Resource {path}: {error}")))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        response
            .error_for_status()
            .and_then(reqwest::blocking::Response::json)
            .map(Some)
            .map_err(|error| execution_error(format!("could not decode Resource {path}: {error}")))
    }

    fn identity_paths(agent_path: &str) -> (String, String, String) {
        let name = agent_path.rsplit('/').next().unwrap_or("agent");
        (
            format!("{AGENT_ROOT}/service-accounts/{name}"),
            format!("{AGENT_ROOT}/links/agents/{name}-runtime-role"),
            format!("{AGENT_ROOT}/links/agents/{name}-service-account"),
        )
    }

    fn identity_mutations(&self, agent: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let (account_path, role_link_path, identity_link_path) = Self::identity_paths(&agent.path);
        let mut mutations = Vec::new();
        if agent.metadata.state == kas_core::STATE_DELETED {
            for path in [&identity_link_path, &role_link_path, &account_path] {
                if let Some(resource) = self.resource(path)? {
                    if resource.metadata.state != kas_core::STATE_DELETED {
                        mutations.push(Mutation::DeleteResource {
                            resource_path: resource.path.clone(),
                            expected_revision: resource.revision,
                        });
                    }
                }
            }
            return Ok(mutations);
        }

        if self.resource(&account_path)?.is_none() {
            mutations.push(Mutation::CreateResource {
                resource: planned(
                    account_path.clone(),
                    SERVICE_ACCOUNT_MANIFEST,
                    format!("{}-agent", agent.name),
                    serde_json::to_value(ServiceAccountSpec::default())
                        .map_err(|error| execution_error(error.to_string()))?,
                ),
            });
        }
        if self.resource(&role_link_path)?.is_none() {
            mutations.push(Mutation::CreateResource {
                resource: link(
                    role_link_path,
                    ROLE_BINDING_RELATION,
                    account_path.clone(),
                    RUNTIME_ROLE.into(),
                )?,
            });
        }
        if self.resource(&identity_link_path)?.is_none() {
            mutations.push(Mutation::CreateResource {
                resource: link(
                    identity_link_path,
                    SERVICE_ACCOUNT_RELATION,
                    agent.path.clone(),
                    account_path,
                )?,
            });
        }
        Ok(mutations)
    }

    fn issue_credential(&self, subject: &str) -> Result<String, DriverError> {
        reqwest::blocking::Client::new()
            .post(format!("{}/credentials/issue", self.api))
            .bearer_auth(&self.token)
            .json(&json!({"subject": subject}))
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::json::<IssuedCredential>)
            .map(|credential| credential.token)
            .map_err(|error| {
                execution_error(format!(
                    "could not issue scoped Agent credential for {subject}: {error}"
                ))
            })
    }

    fn run_codex(
        &self,
        agent: &Resource,
        working_directory: &Path,
        prompt: &str,
        token: &str,
        service_account: &str,
    ) -> Result<String, DriverError> {
        if !working_directory.is_dir() {
            return Err(execution_error(format!(
                "working directory {} does not exist or is not a directory",
                working_directory.display()
            )));
        }
        let context = format!(
            "You are Forge Agent {agent_path}, running inside KAS (Kas Agent System). KAS is a \
resource control plane: every domain object, permission, Package, Manifest, Driver, Role and Link \
is a Resource with a stable path. Your scoped identity is {service_account}. You may inspect KAS \
through $KAS_API with Bearer $KAS_TOKEN, but you do not have standing permission to install a \
Package. If the task needs a new KAS capability, build a .kas tar archive whose manifest is at \
/packages/{{publisher}}/{{package}}/manifest and whose packaged Resource paths use ./...; then submit \
it for human approval with:\n\n  curl -sS -X POST -H \"Authorization: Bearer $KAS_TOKEN\" \\\n+    -H \"Content-Type: application/vnd.kas.manifest+tar\" \\\n+    -H \"X-KAS-Reason: <why this capability is needed>\" \\\n+    --data-binary @<bundle.kas> \"$KAS_PACKAGE_REQUEST_API/package-requests\"\n\nThe \
request is auditable and the Package is installed only after a User approves it.",
            agent_path = agent.path,
        );
        let full_prompt = format!("{context}\n\nEngineering task:\n{prompt}");
        let mut command = Command::new(&self.codex);
        command
            .arg("--ask-for-approval")
            .arg("never")
            .arg("--sandbox")
            .arg("workspace-write")
            .arg("-c")
            .arg("sandbox_workspace_write.network_access=true")
            .arg("exec")
            .arg("-C")
            .arg(working_directory)
            .arg("--skip-git-repo-check")
            .arg("--json")
            .arg("-")
            .current_dir(working_directory)
            .env_remove("KAS_DRIVER_TOKEN")
            .env_remove("KAS_DRIVER_PATH")
            .env_remove("KAS_DRIVER_GENERATION")
            .env_remove("KAS_MANIFEST_PATH")
            .env_remove("KAS_PACKAGE_ROOT")
            .env("KAS_API", &self.api)
            .env("KAS_TOKEN", token)
            .env("KAS_AGENT_PATH", &agent.path)
            .env("KAS_SERVICE_ACCOUNT_PATH", service_account)
            .env("KAS_PACKAGE_REQUEST_API", &self.package_request_api)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(codex_home) = &self.codex_home {
            command.env("CODEX_HOME", codex_home);
        }
        let mut child = command.spawn().map_err(|error| {
            execution_error(format!(
                "could not start Codex executable {}: {error}",
                self.codex.display()
            ))
        })?;
        child
            .stdin
            .take()
            .ok_or_else(|| execution_error("Codex stdin was unavailable"))?
            .write_all(full_prompt.as_bytes())
            .map_err(|error| execution_error(format!("could not write Codex prompt: {error}")))?;
        let output = child
            .wait_with_output()
            .map_err(|error| execution_error(format!("could not wait for Codex: {error}")))?;
        if !output.status.success() {
            return Err(execution_error(format!(
                "Codex exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(final_agent_message(&output.stdout)
            .unwrap_or_else(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
    }
}

#[async_trait]
impl Driver for ForgeAgentDriver {
    fn name(&self) -> &str {
        "forge-agent"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let driver = self.clone();
        let resource = resource.clone();
        tokio::task::spawn_blocking(move || driver.reconcile_blocking(&resource))
            .await
            .map_err(|error| execution_error(format!("Agent reconciliation panicked: {error}")))?
    }

    async fn execute(
        &self,
        resource: &Resource,
        action: &Resource,
        run: &Resource,
    ) -> Result<DriverExecution, DriverError> {
        if action.path != AGENT_ACTION {
            return Err(DriverError::UnsupportedAction(action.path.clone()));
        }
        let driver = self.clone();
        let agent = resource.clone();
        let run = run.clone();
        tokio::task::spawn_blocking(move || {
            let spec: AgentSpec = serde_json::from_value(agent.spec.clone())
                .map_err(|error| execution_error(format!("invalid Agent spec: {error}")))?;
            let run_spec: RunSpec = serde_json::from_value(run.spec)
                .map_err(|error| execution_error(format!("invalid Run spec: {error}")))?;
            let prompt = run_spec
                .input
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| execution_error("Agent Run requires input.prompt"))?;
            let (service_account, _, _) = Self::identity_paths(&agent.path);
            let token = driver.issue_credential(&service_account)?;
            let response = driver.run_codex(
                &agent,
                &spec.working_directory,
                prompt,
                &token,
                &service_account,
            )?;
            Ok(DriverExecution::from(json!({"response": response})))
        })
        .await
        .map_err(|error| execution_error(format!("Agent task panicked: {error}")))?
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api = env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let driver_path = env::var("KAS_DRIVER_PATH")?;
    let generation = env::var("KAS_DRIVER_GENERATION")?.parse()?;
    let token = env::var("KAS_DRIVER_TOKEN")?;
    let codex = env::var("KAS_CODEX_BIN").unwrap_or_else(|_| "codex".into());
    let driver = ForgeAgentDriver {
        api: api.trim_end_matches('/').into(),
        token: token.clone(),
        codex: codex.into(),
        codex_home: env::var_os("KAS_CODEX_HOME").map(PathBuf::from),
        package_request_api: env::var("KAS_PACKAGE_REQUEST_API")
            .unwrap_or_else(|_| "http://127.0.0.1:3004".into())
            .trim_end_matches('/')
            .into(),
    };
    DriverRuntime::new(api, driver_path, generation, token, driver)
        .run()
        .await?;
    Ok(())
}

fn planned(path: String, manifest: &str, name: String, spec: Value) -> PlannedResource {
    PlannedResource {
        path,
        metadata: PlannedResourceMetadata {
            manifest: manifest.into(),
            name,
            state: String::new(),
        },
        spec,
        status: ResourceStatus::default(),
    }
}

fn link(
    path: String,
    relation: &str,
    source: String,
    target: String,
) -> Result<PlannedResource, DriverError> {
    let name = path.rsplit('/').next().unwrap_or("link").to_owned();
    Ok(planned(
        path,
        LINK_MANIFEST,
        name,
        serde_json::to_value(LinkSpec {
            relation: relation.into(),
            source,
            target,
            metadata: json!({}),
        })
        .map_err(|error| execution_error(error.to_string()))?,
    ))
}

fn final_agent_message(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .rev()
        .find_map(|line| {
            let event: Value = serde_json::from_str(line).ok()?;
            let item = event.get("item")?;
            (event.get("type")?.as_str()? == "item.completed"
                && item.get("type")?.as_str()? == "agent_message")
                .then(|| item.get("text")?.as_str().map(str::to_owned))?
        })
}

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}

#[cfg(test)]
mod tests {
    use super::final_agent_message;

    #[test]
    fn extracts_last_codex_agent_message() {
        let output = br#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}
"#;
        assert_eq!(final_agent_message(output).as_deref(), Some("done"));
    }
}
