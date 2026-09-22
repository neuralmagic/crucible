#[tokio::main]
async fn main() -> anyhow::Result<()> {
    crux::cli::run().await
}
