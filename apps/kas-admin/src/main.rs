use std::env;

use kas_store::Store;

fn main() -> anyhow::Result<()> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_default();
    if command != "bootstrap" {
        anyhow::bail!("usage: kas-admin bootstrap [name]");
    }
    let name = arguments.next().unwrap_or_else(|| "admin".into());
    let database = env::var("KAS_DATABASE").unwrap_or_else(|_| ".data/kas.db".into());
    let credential = Store::open_database(&database)?.bootstrap_admin(&name)?;
    println!("{}", credential.token);
    Ok(())
}
