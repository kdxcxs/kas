#[tokio::main]
async fn main() -> anyhow::Result<()> {
    kas_builtin_driver::run_builtin_driver().await
}
