#![cfg(not(test))]

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn take_flag_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let workspace = take_flag_value(&args, "--workspace").context("missing --workspace")?;
    let workspace = PathBuf::from(workspace);
    if !workspace.is_absolute() {
        bail!("--workspace must be an absolute path, got: {}", workspace.display());
    }

    canon_mini_agent::set_workspace(workspace.display().to_string());
    if canon_mini_agent::has_flag(&args, "--graph-complexity-only") {
        let path = canon_mini_agent::complexity::write_graph_only_complexity_report(&workspace)?;
        println!("{}", path.display());
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--artifact-writer-only") {
        let _ = canon_mini_agent::graph_metrics::generate_artifact_writer_dispersion_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--alpha-only") {
        let _ = canon_mini_agent::refactor_analysis::generate_alpha_pathway_issues(&workspace)?;
        Ok(())
    } else {
        canon_mini_agent::complexity::refresh_issue_artifacts(&workspace)
    }
}
