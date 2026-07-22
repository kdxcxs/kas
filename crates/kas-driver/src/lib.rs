use std::time::Duration;

use kas_core::{
    Driver as DriverRecord, DriverState, DriverWork, FinishRun, Resource, Run, RunResult,
    UpdateResourceStatus,
};
use reqwest::Client;
use serde_json::{json, Value};
use thiserror::Error;
use uuid::Uuid;

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

pub struct DriverRuntime<D> {
    api: String,
    driver_id: Uuid,
    generation: u64,
    implementation: D,
    client: Client,
    poll_interval: Duration,
}

impl<D: Driver> DriverRuntime<D> {
    pub fn new(
        api: impl Into<String>,
        driver_id: Uuid,
        generation: u64,
        implementation: D,
    ) -> Self {
        Self {
            api: api.into().trim_end_matches('/').to_owned(),
            driver_id,
            generation,
            implementation,
            client: Client::new(),
            poll_interval: Duration::from_millis(100),
        }
    }

    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    pub async fn run(self) -> anyhow::Result<()> {
        self.mark_ready().await?;
        loop {
            let driver = self.get_driver().await?;
            if driver.generation != self.generation {
                anyhow::bail!(
                    "Driver generation {} was superseded by {}",
                    self.generation,
                    driver.generation
                );
            }
            match driver.state {
                DriverState::Ready => {
                    if let Err(error) = self.tick().await {
                        let is_conflict = error
                            .downcast_ref::<reqwest::Error>()
                            .and_then(reqwest::Error::status)
                            == Some(reqwest::StatusCode::CONFLICT);
                        if !is_conflict {
                            return Err(error);
                        }
                    }
                }
                DriverState::Stopping => {
                    self.mark_stopped().await?;
                    return Ok(());
                }
                state => anyhow::bail!("Driver entered unexpected state {state:?}"),
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn get_driver(&self) -> anyhow::Result<DriverRecord> {
        Ok(self
            .client
            .get(format!("{}/drivers/{}", self.api, self.driver_id))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn mark_ready(&self) -> anyhow::Result<DriverRecord> {
        Ok(self
            .client
            .patch(format!("{}/drivers/{}", self.api, self.driver_id))
            .json(&json!({
                "state": "ready",
                "generation": self.generation,
                "process_id": std::process::id(),
                "metadata": { "implementation": self.implementation.name() }
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn mark_stopped(&self) -> anyhow::Result<DriverRecord> {
        Ok(self
            .client
            .patch(format!("{}/drivers/{}", self.api, self.driver_id))
            .json(&json!({
                "state": "stopped",
                "generation": self.generation
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn tick(&self) -> anyhow::Result<()> {
        let work: Option<DriverWork> = self
            .client
            .post(format!("{}/drivers/{}/claim", self.api, self.driver_id))
            .json(&json!({ "generation": self.generation }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        match work {
            Some(DriverWork::Reconcile { resource, revision }) => {
                self.reconcile_resource(resource, revision).await?;
            }
            Some(DriverWork::Run { run, resource }) => {
                self.execute_run(resource, *run).await?;
            }
            None => {}
        }
        Ok(())
    }

    async fn reconcile_resource(&self, resource: Resource, revision: u64) -> anyhow::Result<()> {
        let status = match self.implementation.reconcile(&resource) {
            Ok(status) => status,
            Err(error) => json!({ "error": error.to_string() }),
        };
        self.client
            .put(format!("{}/resources/{}/status", self.api, resource.id))
            .json(&UpdateResourceStatus {
                driver_id: self.driver_id,
                driver_generation: self.generation,
                observed_revision: revision,
                status,
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn execute_run(&self, resource: Resource, run: Run) -> anyhow::Result<()> {
        let result = match self.implementation.execute(&resource, &run, &mut |_| {}) {
            Ok(output) => RunResult::Succeeded { output },
            Err(error) => RunResult::Failed {
                error: error.to_string(),
            },
        };
        let finished = self
            .client
            .put(format!("{}/runs/{}/result", self.api, run.id))
            .json(&FinishRun {
                driver_generation: self.generation,
                result,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let _: Run = finished;
        Ok(())
    }
}
