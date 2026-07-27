use async_trait::async_trait;
use kas_core::{ActionSpec, DriverExecution, Mutation, Resource, ResourceStatus, RunSpec};
use kas_driver::{Driver, DriverError};
use serde_json::json;

pub struct TestDriver;

#[async_trait]
impl Driver for TestDriver {
    fn name(&self) -> &str {
        "test-driver"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        if resource.manifest != "/manifests/echo" {
            return Ok(Vec::new());
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

    async fn execute(
        &self,
        _resource: &Resource,
        action: &Resource,
        run: &Resource,
    ) -> Result<DriverExecution, DriverError> {
        let _action_spec: ActionSpec = serde_json::from_value(action.spec.clone())
            .map_err(|error| DriverError::Execution(error.to_string()))?;
        let run_spec: RunSpec = serde_json::from_value(run.spec.clone())
            .map_err(|error| DriverError::Execution(error.to_string()))?;
        if action.name != "echo" {
            return Err(DriverError::UnsupportedAction(action.path.clone()));
        }
        Ok(json!({ "echo": run_spec.input }).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_uses_hydrated_action() {
        let resource: Resource = serde_json::from_value(json!({
            "path": "/resources/source",
            "metadata": {
                "manifest": "/manifests/echo",
                "name": "source",
                "state": "available",
                "[kas]": {
                    "revision": 0,
                    "observed": {},
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z"
                }
            },
            "spec": {},
            "status": {"metadata": {"state": "available"}, "spec": {}}
        }))
        .unwrap();
        let run: Resource = serde_json::from_value(json!({
            "path": "/runs/echo-1",
            "metadata": {
                "manifest": "/builtin/run",
                "name": "echo-1",
                "state": "queued",
                "[kas]": {
                    "revision": 1,
                    "observed": {},
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:01Z"
                }
            },
            "spec": {
                "request_id": "10000000-0000-0000-0000-000000000001",
                "resource": "/resources/source",
                "action": "/manifests/echo/actions/echo",
                "driver": "/manifests/echo/driver",
                "input": {"message": "hello"}
            },
            "status": {
                "metadata": {"state": "running"},
                "spec": {
                    "request_id": "10000000-0000-0000-0000-000000000001",
                    "resource": "/resources/source",
                    "action": "/manifests/echo/actions/echo",
                    "driver": "/manifests/echo/driver",
                    "input": {"message": "hello"}
                }
            }
        }))
        .unwrap();
        let action: Resource = serde_json::from_value(json!({
            "path": "/manifests/test/actions/echo",
            "metadata": {
                "manifest": "/builtin/action",
                "name": "echo",
                "state": "available",
                "[kas]": {
                    "revision": 0,
                    "observed": {},
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z"
                }
            },
            "spec": {
                "description": "Echo",
                "input_schema": {},
                "output_schema": {}
            },
            "status": {
                "metadata": {"state": "available"},
                "spec": {
                    "description": "Echo",
                    "input_schema": {},
                    "output_schema": {}
                }
            }
        }))
        .unwrap();

        let execution = TestDriver.execute(&resource, &action, &run).await.unwrap();

        assert_eq!(execution.output["echo"]["message"], "hello");
        assert!(execution.mutations.is_empty());
    }
}
