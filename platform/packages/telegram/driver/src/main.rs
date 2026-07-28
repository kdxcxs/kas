use std::env;

use kas_driver::DriverRuntime;
use kas_telegram_driver::TelegramDriver;

fn main() -> anyhow::Result<()> {
    let api = env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let driver_path = env::var("KAS_DRIVER_PATH")?;
    let generation = env::var("KAS_DRIVER_GENERATION")?.parse()?;
    let token = env::var("KAS_DRIVER_TOKEN")?;
    let approval_api =
        env::var("KAS_APPROVAL_API").unwrap_or_else(|_| "http://127.0.0.1:3003".into());
    let driver = TelegramDriver::new(&api, &token).with_approval_api(approval_api);
    let poller = driver.clone();
    std::thread::spawn(move || poller.poll_forever());
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(DriverRuntime::new(api, driver_path, generation, token, driver).run())
}
