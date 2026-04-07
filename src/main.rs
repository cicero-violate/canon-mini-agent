mod prompts;
mod reports;
mod logging;
mod tools;
mod engine;
mod app;
mod constants;
mod protocol;
mod md_convert;
mod invalid_action;
mod state_space;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::run().await
}
