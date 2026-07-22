use kas_core::{Resource, Run};
use serde_json::Value;
use thiserror::Error;

pub struct DriverEvent {
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("unsupported action: {0}")]
    UnsupportedAction(String),
    #[error("driver execution failed: {0}")]
    Execution(String),
}

pub trait Driver: Send + Sync {
    fn name(&self) -> &str;

    fn reconcile(&self, resource: &Resource) -> Result<Value, DriverError>;

    fn execute(
        &self,
        resource: &Resource,
        run: &Run,
        emit: &mut dyn FnMut(DriverEvent),
    ) -> Result<Value, DriverError>;
}
