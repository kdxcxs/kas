use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kas_core::{
    DriverExecution, Mutation, PlannedResourceMetadata, Resource, ResourceStatus, UpdateResource,
    UpdateResourceMetadata,
};
use kas_driver::{Driver, DriverError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const APPROVAL_MANIFEST: &str = "/manifests/approval";
pub const APPROVAL_RESULT_MANIFEST: &str = "/manifests/approval-result";
pub const USER_MANIFEST: &str = "/builtin/user";
pub const SERVICE_ACCOUNT_MANIFEST: &str = "/builtin/service-account";
pub const AGENT_MANIFEST: &str = "/manifests/agent";
pub const LINK_MANIFEST: &str = "/builtin/link";
pub const REQUESTED_BY_RELATION: &str = "/manifests/approval/relations/requested-by";
pub const DECIDES_RELATION: &str = "/manifests/approval/relations/decides";
pub const DECIDED_BY_RELATION: &str = "/manifests/approval/relations/decided-by";
pub const RESULT_OF_RELATION: &str = "/manifests/approval/relations/result-of";
pub const PRODUCED_BY_RELATION: &str = "/manifests/approval/relations/produced-by";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum ApprovalOperation {
    Create {
        resource: ApprovalCreateResource,
    },
    Update {
        path: String,
        update: UpdateResource,
    },
    Delete {
        path: String,
        expected_revision: u64,
    },
    Get {
        path: String,
    },
    List {
        manifest: String,
        #[serde(default)]
        path_prefix: Option<String>,
        #[serde(default = "default_list_limit")]
        limit: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalCreateResource {
    pub path: String,
    pub metadata: PlannedResourceMetadata,
    #[serde(default)]
    pub spec: Value,
}

impl ApprovalOperation {
    pub fn scope_path(&self) -> &str {
        match self {
            Self::Create { resource } => &resource.path,
            Self::Update { path, .. } | Self::Delete { path, .. } | Self::Get { path } => path,
            Self::List { path_prefix, .. } => path_prefix.as_deref().unwrap_or("/"),
        }
    }
}

fn default_list_limit() -> u32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalSpec {
    Request {
        reason: String,
        operation: ApprovalOperation,
        expires_at: DateTime<Utc>,
    },
    Decision {
        outcome: DecisionOutcome,
        decided_at: DateTime<Utc>,
        completed_at: Option<DateTime<Utc>>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResultSpec {
    pub response: ApprovalResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOutcome {
    Pending,
    Executing,
    Succeeded,
    Rejected,
    Failed,
    Invalid,
    Superseded,
}

impl DecisionOutcome {
    pub fn state(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Executing => "executing",
            Self::Succeeded => "succeeded",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
            Self::Invalid => "invalid",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalDriver {
    _api: String,
    _token: String,
}

impl ApprovalDriver {
    pub fn new(api: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            _api: api.into().trim_end_matches('/').to_owned(),
            _token: token.into(),
        }
    }

    fn reconcile_blocking(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        if resource.manifest != APPROVAL_MANIFEST {
            return Err(execution_error(format!(
                "Approval Driver cannot reconcile Manifest {}",
                resource.manifest
            )));
        }
        let spec: ApprovalSpec = serde_json::from_value(resource.spec.clone())
            .map_err(|error| execution_error(format!("invalid Approval spec: {error}")))?;
        let desired_state = match &spec {
            ApprovalSpec::Request { expires_at, .. } => {
                if resource.metadata.state == "pending" && *expires_at <= Utc::now() {
                    "expired"
                } else {
                    resource.metadata.state.as_str()
                }
            }
            ApprovalSpec::Decision { outcome, .. } => outcome.state(),
        };

        let mut mutations = Vec::new();
        if resource.metadata.state != desired_state {
            mutations.push(Mutation::UpdateResource {
                resource_path: resource.path.clone(),
                expected_revision: resource.revision,
                metadata: Some(UpdateResourceMetadata {
                    state: desired_state.into(),
                }),
                spec: resource.spec.clone(),
            });
        } else if resource.status.metadata.state != desired_state
            || resource.status.spec != resource.spec
        {
            mutations.push(Mutation::UpdateResourceStatus {
                resource_path: resource.path.clone(),
                expected_revision: resource.revision,
                status: ResourceStatus {
                    metadata: resource.status_metadata(desired_state),
                    spec: resource.spec.clone(),
                },
            });
        }
        Ok(mutations)
    }
}

#[async_trait]
impl Driver for ApprovalDriver {
    fn name(&self) -> &str {
        "approval"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let driver = self.clone();
        let resource = resource.clone();
        tokio::task::spawn_blocking(move || driver.reconcile_blocking(&resource))
            .await
            .map_err(|error| execution_error(format!("Approval worker failed: {error}")))?
    }

    async fn execute(
        &self,
        _resource: &Resource,
        action: &Resource,
        _run: &Resource,
    ) -> Result<DriverExecution, DriverError> {
        Err(DriverError::UnsupportedAction(action.path.clone()))
    }
}

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}

#[cfg(test)]
mod tests {
    use super::{ApprovalCreateResource, ApprovalOperation};
    use kas_core::PlannedResourceMetadata;
    use serde_json::json;

    #[test]
    fn create_operation_uses_the_resource_path() {
        let operation = ApprovalOperation::Create {
            resource: ApprovalCreateResource {
                path: "/messages/proof".into(),
                metadata: PlannedResourceMetadata {
                    manifest: "/manifests/message".into(),
                    name: "proof".into(),
                    state: String::new(),
                },
                spec: json!({"role": "system", "body": "approved"}),
            },
        };
        assert_eq!(operation.scope_path(), "/messages/proof");
    }
}
