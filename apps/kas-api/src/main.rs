use std::env;

use kas_store::Store;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let data_dir = env::var("KAS_DATA_DIR").unwrap_or_else(|_| ".data".into());
    let database = env::var("KAS_DATABASE").unwrap_or_else(|_| format!("{data_dir}/kas.db"));
    if !is_postgres(&database) {
        if let Some(parent) = std::path::Path::new(&database).parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let address = env::var("KAS_ADDRESS").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let default_api_address = address
        .strip_prefix("0.0.0.0:")
        .map(|port| format!("127.0.0.1:{port}"))
        .unwrap_or_else(|| address.clone());
    let api_url =
        env::var("KAS_API_URL").unwrap_or_else(|_| format!("http://{default_api_address}"));
    let app = kas_api::app_with_config(
        Store::open_database(&database)?,
        kas_api::AppConfig {
            data_dir: data_dir.into(),
            api_url,
        },
    );
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("kas-api listening on http://{address}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn is_postgres(database: &str) -> bool {
    database.starts_with("postgres://") || database.starts_with("postgresql://")
}
