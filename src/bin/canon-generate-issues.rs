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
    } else if canon_mini_agent::has_flag(&args, "--graph-verify-snapshot") {
        let path = canon_mini_agent::complexity::write_graph_verification_snapshot(&workspace)?;
        println!("{}", path.display());
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--graph-verify-delta") {
        let path = canon_mini_agent::complexity::write_graph_delta_report(&workspace)?;
        println!("{}", path.display());
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--artifact-writer-only") {
        let _ = canon_mini_agent::graph_metrics::generate_artifact_writer_dispersion_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--error-shaping-only") {
        let _ = canon_mini_agent::graph_metrics::generate_error_shaping_dispersion_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--state-transition-only") {
        let _ = canon_mini_agent::graph_metrics::generate_state_transition_dispersion_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--planner-loop-only") {
        let _ = canon_mini_agent::graph_metrics::generate_planner_loop_fragmentation_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--implicit-state-machine-only") {
        let _ = canon_mini_agent::graph_metrics::generate_implicit_state_machine_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--effect-boundary-only") {
        let _ = canon_mini_agent::graph_metrics::generate_effect_boundary_leak_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--representation-fanout-only") {
        let _ = canon_mini_agent::graph_metrics::generate_representation_fanout_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--cfg-region-only") {
        let _ = canon_mini_agent::graph_metrics::generate_scc_region_reduction_issues(&workspace)?;
        let _ = canon_mini_agent::graph_metrics::generate_dominator_region_reduction_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--scc-region-only") {
        let _ = canon_mini_agent::graph_metrics::generate_scc_region_reduction_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--dominator-region-only") {
        let _ = canon_mini_agent::graph_metrics::generate_dominator_region_reduction_issues(&workspace)?;
        Ok(())
    } else if canon_mini_agent::has_flag(&args, "--alpha-only") {
        let _ = canon_mini_agent::refactor_analysis::generate_alpha_pathway_issues(&workspace)?;
        Ok(())
    } else {
        canon_mini_agent::complexity::refresh_issue_artifacts(&workspace)
    }
}
