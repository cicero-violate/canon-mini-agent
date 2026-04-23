#![cfg(not(test))]

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

fn take_flag_value(args: &[String], name: &str) -> Option<String> {
    let flag_index = args.iter().position(|arg| arg == name)?;
    args.get(flag_index + 1).cloned()
}

fn print_generated_path(result: Result<PathBuf>) -> Result<()> {
    result.map(|path| println!("{}", path.display()))
}

fn run_graph_report_mode(args: &[String], workspace: &PathBuf) -> Option<Result<()>> {
    const GRAPH_REPORT_MODES: &[(&str, fn(&Path) -> Result<PathBuf>)] = &[
        (
            "--graph-complexity-only",
            canon_mini_agent::complexity::write_graph_only_complexity_report,
        ),
        (
            "--graph-verify-snapshot",
            canon_mini_agent::complexity::write_graph_verification_snapshot,
        ),
        (
            "--graph-verify-delta",
            canon_mini_agent::complexity::write_graph_delta_report,
        ),
    ];

    GRAPH_REPORT_MODES.iter().find_map(|(flag, action)| {
        canon_mini_agent::has_flag(args, flag).then(|| print_generated_path(action(workspace)))
    })
}

fn run_graph_issue_mode(args: &[String], workspace: &PathBuf) -> Option<Result<()>> {
    const GRAPH_ISSUE_MODES: &[(&str, fn(&Path) -> Result<usize>)] = &[
        (
            "--artifact-writer-only",
            canon_mini_agent::graph_metrics::generate_artifact_writer_dispersion_issues,
        ),
        (
            "--error-shaping-only",
            canon_mini_agent::graph_metrics::generate_error_shaping_dispersion_issues,
        ),
        (
            "--state-transition-only",
            canon_mini_agent::graph_metrics::generate_state_transition_dispersion_issues,
        ),
        (
            "--planner-loop-only",
            canon_mini_agent::graph_metrics::generate_planner_loop_fragmentation_issues,
        ),
        (
            "--implicit-state-machine-only",
            canon_mini_agent::graph_metrics::generate_implicit_state_machine_issues,
        ),
        (
            "--effect-boundary-only",
            canon_mini_agent::graph_metrics::generate_effect_boundary_leak_issues,
        ),
        (
            "--logging-dispersion-only",
            canon_mini_agent::graph_metrics::generate_logging_dispersion_issues,
        ),
        (
            "--process-spawn-only",
            canon_mini_agent::graph_metrics::generate_process_spawn_dispersion_issues,
        ),
        (
            "--network-usage-only",
            canon_mini_agent::graph_metrics::generate_network_usage_dispersion_issues,
        ),
        (
            "--representation-fanout-only",
            canon_mini_agent::graph_metrics::generate_representation_fanout_issues,
        ),
    ];

    GRAPH_ISSUE_MODES.iter().find_map(|(flag, action)| {
        canon_mini_agent::has_flag(args, flag).then(|| action(workspace).map(|_| ()))
    })
}

fn run_cfg_region_mode(workspace: &Path) -> Result<()> {
    canon_mini_agent::graph_metrics::generate_scc_region_reduction_issues(workspace)
        .and_then(|_| canon_mini_agent::graph_metrics::generate_dominator_region_reduction_issues(workspace))
        .map(|_| ())
}

fn run_simple_issue_mode(args: &[String], workspace: &PathBuf) -> Option<Result<()>> {
    const SIMPLE_ISSUE_MODES: &[(&str, fn(&Path) -> Result<()>)] = &[
        (
            "--scc-region-only",
            |workspace| {
                canon_mini_agent::graph_metrics::generate_scc_region_reduction_issues(workspace)
                    .map(|_| ())
            },
        ),
        (
            "--dominator-region-only",
            |workspace| {
                canon_mini_agent::graph_metrics::generate_dominator_region_reduction_issues(workspace)
                    .map(|_| ())
            },
        ),
        (
            "--alpha-only",
            |workspace| {
                canon_mini_agent::refactor_analysis::generate_alpha_pathway_issues(workspace)
                    .map(|_| ())
            },
        ),
    ];

    SIMPLE_ISSUE_MODES.iter().find_map(|(flag, action)| {
        canon_mini_agent::has_flag(args, flag).then(|| action(workspace))
    })
}

fn run_selected_mode(args: &[String], workspace: &PathBuf) -> Option<Result<()>> {
    if let Some(result) = run_graph_report_mode(args, workspace) {
        Some(result)
    } else if let Some(result) = run_graph_issue_mode(args, workspace) {
        Some(result)
    } else if canon_mini_agent::has_flag(args, "--cfg-region-only") {
        Some(run_cfg_region_mode(workspace))
    } else {
        run_simple_issue_mode(args, workspace)
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
