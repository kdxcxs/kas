use std::env;

fn main() -> anyhow::Result<()> {
    let database = env::var("KAS_DATABASE").unwrap_or_else(|_| ".data/kas.db".into());
    if let Some(parent) = std::path::Path::new(&database).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let version = kas_store::migrate(&database)?;
    println!("database migrated to schema version {version}");
    Ok(())
}
