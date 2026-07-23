use kas_core::{
    DriverExecution, Mutation, ObjectKind, ObjectRef, PlannedLink, PlannedResource, Resource, Run,
};
use kas_driver::{Driver, DriverError};
use serde_json::{json, Value};

pub struct TestDriver;

impl Driver for TestDriver {
    fn name(&self) -> &str {
        "test-driver"
    }

    fn reconcile(&self, resource: &Resource) -> Result<Value, DriverError> {
        Ok(json!({ "observed_spec": resource.spec }))
    }

    fn execute(&self, resource: &Resource, run: &Run) -> Result<DriverExecution, DriverError> {
        if run.action != "echo" {
            return Err(DriverError::UnsupportedAction(run.action.clone()));
        }
        let mut execution: DriverExecution = json!({ "echo": run.input }).into();
        if let Some(manifest_path) = resource
            .spec
            .get("fanout_manifest_path")
            .and_then(Value::as_str)
        {
            let resource_path = format!("{}/fanout", run.path);
            execution.output["fanout_resource_path"] = json!(resource_path);
            execution.mutations = vec![
                Mutation::CreateResource {
                    resource: PlannedResource {
                        path: resource_path.clone(),
                        manifest_path: manifest_path.into(),
                        name: format!("echo-{}", run.path.rsplit('/').next().unwrap_or("result")),
                        spec: json!({ "archived": false }),
                    },
                },
                Mutation::CreateLink {
                    link: PlannedLink {
                        path: format!("{}/links/produces", run.path),
                        source: ObjectRef {
                            kind: ObjectKind::Run,
                            path: run.path.clone(),
                        },
                        relation: "produces".into(),
                        target: ObjectRef {
                            kind: ObjectKind::Resource,
                            path: resource_path,
                        },
                        metadata: json!({}),
                    },
                },
            ];
        }
        Ok(execution)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fanout_uses_run_scoped_paths() {
        let resource: Resource = serde_json::from_value(json!({
            "path": "/resources/source",
            "manifest_path": "/manifests/source",
            "name": "source",
            "spec": {"fanout_manifest_path": "/manifests/message"},
            "status": {},
            "revision": 0,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        let run: Run = serde_json::from_value(json!({
            "path": "/runs/echo-1",
            "request_id": "10000000-0000-0000-0000-000000000001",
            "resource_path": "/resources/source",
            "driver_path": "/drivers/source",
            "driver_generation": 1,
            "action": "echo",
            "input": {"message": "hello"},
            "status": "running",
            "output": null,
            "error": null,
            "created_at": "2026-01-01T00:00:00Z",
            "started_at": "2026-01-01T00:00:01Z",
            "finished_at": null
        }))
        .unwrap();

        let execution = TestDriver.execute(&resource, &run).unwrap();

        assert_eq!(
            execution.output["fanout_resource_path"],
            "/runs/echo-1/fanout"
        );
        assert!(matches!(
            execution.mutations.as_slice(),
            [
                Mutation::CreateResource {
                    resource: PlannedResource {
                        path,
                        manifest_path,
                        ..
                    }
                },
                Mutation::CreateLink {
                    link: PlannedLink {
                        path: link_path,
                        source: ObjectRef {
                            path: source_path,
                            ..
                        },
                        target: ObjectRef {
                            path: target_path,
                            ..
                        },
                        ..
                    }
                }
            ] if path == "/runs/echo-1/fanout"
                && manifest_path == "/manifests/message"
                && link_path == "/runs/echo-1/links/produces"
                && source_path == "/runs/echo-1"
                && target_path == "/runs/echo-1/fanout"
        ));
    }
}
