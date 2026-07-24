use kas_core::{Action, DriverExecution, Mutation, ReconcileObject, Resource, Run};
use kas_driver::{Driver, DriverError};
use serde_json::json;

pub struct TestDriver;

impl Driver for TestDriver {
    fn name(&self) -> &str {
        "test-driver"
    }

    fn reconcile(&self, object: &ReconcileObject) -> Result<Vec<Mutation>, DriverError> {
        Ok(match object {
            ReconcileObject::Resource(resource) => vec![Mutation::UpdateResourceStatus {
                resource_path: resource.path.clone(),
                expected_revision: resource.revision,
                status: resource.spec.clone(),
            }],
            ReconcileObject::Link(link) => vec![Mutation::UpdateLink {
                link_path: link.path.clone(),
                expected_revision: link.revision,
                source: link.source.clone(),
                target: link.target.clone(),
                status: link.spec.clone(),
            }],
        })
    }

    fn execute(
        &self,
        _resource: &Resource,
        action: &Action,
        run: &Run,
    ) -> Result<DriverExecution, DriverError> {
        if action.name != "echo" {
            return Err(DriverError::UnsupportedAction(action.path.clone()));
        }
        Ok(json!({ "echo": run.input }).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_uses_hydrated_action() {
        let resource: Resource = serde_json::from_value(json!({
            "path": "/resources/source",
            "name": "source",
            "spec": {},
            "status": {},
            "revision": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        let run: Run = serde_json::from_value(json!({
            "path": "/runs/echo-1",
            "request_id": "10000000-0000-0000-0000-000000000001",
            "driver_generation": 1,
            "input": {"message": "hello"},
            "status": "running",
            "output": null,
            "error": null,
            "created_at": "2026-01-01T00:00:00Z",
            "started_at": "2026-01-01T00:00:01Z",
            "finished_at": null
        }))
        .unwrap();
        let action: Action = serde_json::from_value(json!({
            "path": "/manifests/test/actions/echo",
            "name": "echo",
            "description": "Echo",
            "input_schema": {},
            "output_schema": {}
        }))
        .unwrap();

        let execution = TestDriver.execute(&resource, &action, &run).unwrap();

        assert_eq!(execution.output["echo"]["message"], "hello");
        assert!(execution.mutations.is_empty());
    }
}
