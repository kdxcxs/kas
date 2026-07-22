use std::env;

use kas_store::Store;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let database = env::var("KAS_DATABASE").unwrap_or_else(|_| ".data/kas.db".into());
    if let Some(parent) = std::path::Path::new(&database).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let app = kas_api::app(Store::open(database)?);
    let address = env::var("KAS_ADDRESS").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("kas-api listening on http://{address}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
