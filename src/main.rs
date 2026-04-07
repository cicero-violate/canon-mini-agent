mod prompts;
mod reports;
mod logging;
mod tools;
mod engine;
mod app;
mod constants;
mod protocol;
mod md_convert;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::run().await
}
