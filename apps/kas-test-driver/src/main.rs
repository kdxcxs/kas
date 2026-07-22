use std::env;

use kas_driver::DriverRuntime;
use kas_test_driver::TestDriver;
use uuid::Uuid;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api = env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let driver_id = Uuid::parse_str(&env::var("KAS_DRIVER_ID")?)?;
    let generation = env::var("KAS_DRIVER_GENERATION")?.parse()?;
    DriverRuntime::new(api, driver_id, generation, TestDriver)
        .run()
        .await
}
