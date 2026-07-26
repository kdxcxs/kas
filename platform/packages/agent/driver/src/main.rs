use std::{env, path::PathBuf};

use kas_agent_driver::AgentDriver;
use kas_driver::DriverRuntime;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api = env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let driver_path = env::var("KAS_DRIVER_PATH")?;
    let generation = env::var("KAS_DRIVER_GENERATION")?.parse()?;
    let token = env::var("KAS_DRIVER_TOKEN")?;
    let codex = env::var_os("KAS_CODEX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"));
    let mut driver = AgentDriver::new(&api, &token, codex);
    if let Some(codex_home) = env::var_os("KAS_CODEX_HOME") {
        driver = driver.with_codex_home(PathBuf::from(codex_home));
    }
    DriverRuntime::new(api, driver_path, generation, token, driver)
        .run()
        .await
}
