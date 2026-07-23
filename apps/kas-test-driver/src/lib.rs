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
        if let Some(manifest_id) = resource
            .spec
            .get("fanout_manifest_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse().ok())
        {
            let resource_id = uuid::Uuid::new_v4();
            execution.output["fanout_resource_id"] = json!(resource_id);
            execution.mutations = vec![
                Mutation::CreateResource {
                    resource: PlannedResource {
                        id: resource_id,
                        manifest_id,
                        name: format!("echo-{}", run.id),
                        spec: json!({ "archived": false }),
                    },
                },
                Mutation::CreateLink {
                    link: PlannedLink {
                        id: uuid::Uuid::new_v4(),
                        source: ObjectRef {
                            kind: ObjectKind::Run,
                            id: run.id,
                        },
                        relation: "produces".into(),
                        target: ObjectRef {
                            kind: ObjectKind::Resource,
                            id: resource_id,
                        },
                        metadata: json!({}),
                    },
                },
            ];
        }
        Ok(execution)
    }
}
