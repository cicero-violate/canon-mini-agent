#[tokio::main]
async fn main() -> anyhow::Result<()> {
    canon_mini_agent::app::run().await
}
