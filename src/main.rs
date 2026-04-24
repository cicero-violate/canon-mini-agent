#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("semantic-manifest") {
        let workspace = std::env::current_dir()?;
        let _ = canon_mini_agent::semantic_manifest::run_from_cli_args(&args[2..], workspace)?;
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("syn-writer") {
        let workspace = std::env::current_dir()?;
        let _ = canon_mini_agent::syn_writer::run_from_cli_args(&args[2..], workspace)?;
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("semantic-rank-candidates") {
        let workspace = std::env::current_dir()?;
        let _ =
            canon_mini_agent::semantic_rank_candidates::run_from_cli_args(&args[2..], workspace)?;
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("semantic-project-issues") {
        let workspace = std::env::current_dir()?;
        let _ =
            canon_mini_agent::semantic_issue_projection::project_from_cli_args(&args[2..], workspace.as_path())?;
        return Ok(());
    }
    canon_mini_agent::app::run().await
}
