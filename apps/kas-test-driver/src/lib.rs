use kas_core::{ActionSpec, DriverExecution, Mutation, Resource, RunSpec};
use kas_driver::{Driver, DriverError};
use serde_json::json;

pub struct TestDriver;

impl Driver for TestDriver {
    fn name(&self) -> &str {
        "test-driver"
    }

    fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        Ok(vec![Mutation::UpdateResourceStatus {
            resource_path: resource.path.clone(),
            expected_revision: resource.revision,
            status: resource.spec.clone(),
        }])
    }

    fn execute(
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

    #[test]
    fn echo_uses_hydrated_action() {
        let resource: Resource = serde_json::from_value(json!({
            "path": "/resources/source",
            "manifest": "/manifests/echo",
            "name": "source",
            "spec": {},
            "status": {},
            "revision": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        let run: Resource = serde_json::from_value(json!({
            "path": "/runs/echo-1",
            "manifest": "/builtin/run",
            "name": "echo-1",
            "spec": {
                "request_id": "10000000-0000-0000-0000-000000000001",
                "resource": "/resources/source",
                "action": "/manifests/echo/actions/echo",
                "driver": "/manifests/echo/driver",
                "input": {"message": "hello"}
            },
            "status": {"state": "running", "driver_generation": 1},
            "revision": 1,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:01Z"
        }))
        .unwrap();
        let action: Resource = serde_json::from_value(json!({
            "path": "/manifests/test/actions/echo",
            "manifest": "/builtin/action",
            "name": "echo",
            "spec": {
                "description": "Echo",
                "input_schema": {},
                "output_schema": {}
            },
            "status": {},
            "revision": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();

        let execution = TestDriver.execute(&resource, &action, &run).unwrap();

        assert_eq!(execution.output["echo"]["message"], "hello");
        assert!(execution.mutations.is_empty());
    }
}
