use std::env;

use kas_driver::DriverRuntime;
use kas_test_driver::TestDriver;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api = env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let driver_path = env::var("KAS_DRIVER_PATH")?;
    let generation = env::var("KAS_DRIVER_GENERATION")?.parse()?;
    let token = env::var("KAS_DRIVER_TOKEN")?;
    DriverRuntime::new(api, driver_path, generation, token, TestDriver)
        .run()
        .await
}
