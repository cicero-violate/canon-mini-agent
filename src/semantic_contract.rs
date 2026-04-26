use anyhow::Context;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct SemanticManifestMetrics {
    pub fn_total: usize,
    pub fn_with_any_error: usize,
    pub fn_error_rate: f64,
    pub fn_intent_classified: usize,
    pub fn_low_confidence: usize,
    pub fn_intent_coverage: f64,
    pub fn_low_confidence_rate: f64,
}

impl SemanticManifestMetrics {
    pub fn score(&self) -> f64 {
        let hard_error_score = (1.0 - self.fn_error_rate).clamp(0.0, 1.0);
        let coverage_score = self.fn_intent_coverage.clamp(0.0, 1.0);
        let confidence_score = (1.0 - self.fn_low_confidence_rate).clamp(0.0, 1.0);
        (hard_error_score * coverage_score * confidence_score).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SemanticSyncReport {
    pub metrics: SemanticManifestMetrics,
    pub rewrites_applied: bool,
    pub rank_candidates_total: usize,
    pub rank_safe_merge: usize,
    pub rank_investigate: usize,
    pub rank_skip: usize,
    pub rank_unmatched_owners: usize,
    pub projected_issue_candidates: usize,
    pub projected_rank_issue_candidates: usize,
    pub projected_manifest_issue_candidates: usize,
    pub projected_issue_upserts: usize,
    pub projected_issue_resolved: usize,
    pub projected_issue_rewrote: bool,
}

#[derive(serde::Deserialize, Default)]
struct ProposalFile {
    #[serde(default)]
    fn_total: usize,
    #[serde(default)]
    fn_with_any_error: usize,
    #[serde(default)]
    fn_error_rate: f64,
    #[serde(default)]
    fn_intent_classified: usize,
    #[serde(default)]
    fn_low_confidence: usize,
    #[serde(default)]
    fn_intent_coverage: f64,
    #[serde(default)]
    fn_low_confidence_rate: f64,
}

pub fn sidecar_path(workspace: &Path) -> PathBuf {
    workspace
        .join("agent_state")
        .join("semantic_manifest_proposals.json")
}

pub fn graph_path(workspace: &Path) -> PathBuf {
    workspace
        .join("state")
        .join("rustc")
        .join("canon_mini_agent")
        .join("graph.json")
}

pub fn graph_content_fingerprint(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read graph fingerprint input {}", path.display()))?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(format!("len={} hash={:016x}", bytes.len(), hasher.finish()))
}

pub fn rank_out_path(workspace: &Path) -> PathBuf {
    workspace
        .join("agent_state")
        .join("safe_patch_candidates.json")
}

pub fn load_semantic_manifest_metrics(workspace: &Path) -> SemanticManifestMetrics {
    let path = sidecar_path(workspace);
    let Ok(raw) = std::fs::read(&path) else {
        return SemanticManifestMetrics::default();
    };
    let Ok(file) = serde_json::from_slice::<ProposalFile>(&raw) else {
        return SemanticManifestMetrics::default();
    };
    let intent_coverage = if file.fn_total > 0
        && file.fn_intent_coverage == 0.0
        && file.fn_intent_classified == 0
        && file.fn_low_confidence == 0
    {
        1.0
    } else {
        file.fn_intent_coverage.clamp(0.0, 1.0)
    };
    SemanticManifestMetrics {
        fn_total: file.fn_total,
        fn_with_any_error: file.fn_with_any_error,
        fn_error_rate: file.fn_error_rate.clamp(0.0, 1.0),
        fn_intent_classified: file.fn_intent_classified,
        fn_low_confidence: file.fn_low_confidence,
        fn_intent_coverage: intent_coverage,
        fn_low_confidence_rate: file.fn_low_confidence_rate.clamp(0.0, 1.0),
    }
}

fn file_modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

pub fn semantic_sync_outputs_stale(workspace: &Path) -> bool {
    let graph = graph_path(workspace);
    let sidecar = sidecar_path(workspace);
    let rank_out = rank_out_path(workspace);
    let Some(graph_mtime) = file_modified(&graph) else {
        return false;
    };
    for artifact in [&sidecar, &rank_out] {
        let Some(artifact_mtime) = file_modified(artifact) else {
            return true;
        };
        if artifact_mtime < graph_mtime {
            return true;
        }
    }
    false
}

pub fn run_semantic_sync(workspace: &Path) -> Result<SemanticSyncReport, anyhow::Error> {
    let graph = graph_path(workspace);
    let sidecar = sidecar_path(workspace);
    let rank_out = rank_out_path(workspace);
    let max_error_rate = std::env::var("SEM_MAX_ERROR_RATE").ok();
    let apply_rewrites = std::env::var("CANON_SEMANTIC_REWRITE")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);

    if let Some(parent) = sidecar.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Some(parent) = rank_out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let max_error_rate_value = max_error_rate
        .as_deref()
        .and_then(|v| v.parse::<f64>().ok());
    crate::semantic_manifest::run_with_options(
        crate::semantic_manifest::SemanticManifestRunOptions {
            workspace: workspace.to_path_buf(),
            graph_path: graph.clone(),
            out_path: sidecar.clone(),
            write_mode: true,
            max_error_rate: max_error_rate_value,
        },
    )?;

    if apply_rewrites {
        crate::syn_writer::run_with_options(crate::syn_writer::SynWriterRunOptions {
            workspace_root: workspace.to_path_buf(),
            graph_path: graph.clone(),
            manifest_path: sidecar.clone(),
            log_path: workspace.join("agent_state").join("syn_writer_log.json"),
            write_mode: true,
            augment: false,
            rewrite_existing: true,
        })?;

        // Refresh sidecar after rewrite.
        crate::semantic_manifest::run_with_options(
            crate::semantic_manifest::SemanticManifestRunOptions {
                workspace: workspace.to_path_buf(),
                graph_path: graph.clone(),
                out_path: sidecar.clone(),
                write_mode: true,
                max_error_rate: max_error_rate_value,
            },
        )?;
    }

    let rank_report = crate::semantic_rank_candidates::run_with_options(
        crate::semantic_rank_candidates::SemanticRankCandidatesOptions {
            workspace_root: workspace.to_path_buf(),
            graph_path: graph.clone(),
            out_path: Some(rank_out),
        },
    )?;
    let issue_projection = crate::semantic_issue_projection::project_semantic_rank_issues(
        workspace,
        rank_report.out_path.as_path(),
    )?;

    Ok(SemanticSyncReport {
        metrics: load_semantic_manifest_metrics(workspace),
        rewrites_applied: apply_rewrites,
        rank_candidates_total: rank_report.candidates,
        rank_safe_merge: rank_report.safe_merge,
        rank_investigate: rank_report.investigate,
        rank_skip: rank_report.skip,
        rank_unmatched_owners: rank_report.unmatched_owners,
        projected_issue_candidates: issue_projection.selected_candidates,
        projected_rank_issue_candidates: issue_projection.rank_selected_candidates,
        projected_manifest_issue_candidates: issue_projection.manifest_selected_candidates,
        projected_issue_upserts: issue_projection.opened_or_updated,
        projected_issue_resolved: issue_projection.resolved_stale,
        projected_issue_rewrote: issue_projection.rewrote_issues,
    })
}
