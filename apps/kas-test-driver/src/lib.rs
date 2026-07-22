use kas_core::{Resource, Run};
use kas_driver::{Driver, DriverError, DriverEvent};
use serde_json::{json, Value};

pub struct TestDriver;

impl Driver for TestDriver {
    fn name(&self) -> &str {
        "test-driver"
    }

    fn reconcile(&self, resource: &Resource) -> Result<Value, DriverError> {
        Ok(json!({ "observed_spec": resource.spec }))
    }

    fn execute(
        &self,
        _resource: &Resource,
        run: &Run,
        emit: &mut dyn FnMut(DriverEvent),
    ) -> Result<Value, DriverError> {
        if run.action != "echo" {
            return Err(DriverError::UnsupportedAction(run.action.clone()));
        }
        emit(DriverEvent {
            kind: "test_driver.echoed".into(),
            data: run.input.clone(),
        });
        Ok(json!({ "echo": run.input }))
    }
}
