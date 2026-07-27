use std::env;

fn main() -> anyhow::Result<()> {
    let database = env::var("KAS_DATABASE").unwrap_or_else(|_| ".data/kas.db".into());
    if !is_postgres(&database) {
        if let Some(parent) = std::path::Path::new(&database).parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let version = kas_store::migrate_database(&database)?;
    println!("database migrated to schema version {version}");
    Ok(())
}

fn is_postgres(database: &str) -> bool {
    database.starts_with("postgres://") || database.starts_with("postgresql://")
}
