#![cfg(not(test))]

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn take_flag_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

fn run_selected_mode(args: &[String], workspace: &PathBuf) -> Option<Result<()>> {
    if canon_mini_agent::has_flag(args, "--graph-complexity-only") {
        let path = canon_mini_agent::complexity::write_graph_only_complexity_report(workspace);
        Some(path.map(|path| println!("{}", path.display())))
    } else if canon_mini_agent::has_flag(args, "--graph-verify-snapshot") {
        let path = canon_mini_agent::complexity::write_graph_verification_snapshot(workspace);
        Some(path.map(|path| println!("{}", path.display())))
    } else if canon_mini_agent::has_flag(args, "--graph-verify-delta") {
        let path = canon_mini_agent::complexity::write_graph_delta_report(workspace);
        Some(path.map(|path| println!("{}", path.display())))
    } else if canon_mini_agent::has_flag(args, "--artifact-writer-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_artifact_writer_dispersion_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--error-shaping-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_error_shaping_dispersion_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--state-transition-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_state_transition_dispersion_issues(
                workspace,
            )
            .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--planner-loop-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_planner_loop_fragmentation_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--implicit-state-machine-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_implicit_state_machine_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--effect-boundary-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_effect_boundary_leak_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--logging-dispersion-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_logging_dispersion_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--process-spawn-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_process_spawn_dispersion_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--network-usage-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_network_usage_dispersion_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--representation-fanout-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_representation_fanout_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--cfg-region-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_scc_region_reduction_issues(workspace)
                .and_then(|_| {
                    canon_mini_agent::graph_metrics::generate_dominator_region_reduction_issues(
                        workspace,
                    )
                })
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--scc-region-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_scc_region_reduction_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--dominator-region-only") {
        Some(
            canon_mini_agent::graph_metrics::generate_dominator_region_reduction_issues(workspace)
                .map(|_| ()),
        )
    } else if canon_mini_agent::has_flag(args, "--alpha-only") {
        Some(
            canon_mini_agent::refactor_analysis::generate_alpha_pathway_issues(workspace)
                .map(|_| ()),
        )
    } else {
        None
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let workspace = take_flag_value(&args, "--workspace").context("missing --workspace")?;
    let workspace = PathBuf::from(workspace);
    if !workspace.is_absolute() {
        bail!("--workspace must be an absolute path, got: {}", workspace.display());
    }

    canon_mini_agent::set_workspace(workspace.display().to_string());
    if let Some(result) = run_selected_mode(&args, &workspace) {
        result
    } else {
        canon_mini_agent::complexity::refresh_issue_artifacts(&workspace)
    }
}
