use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct SemanticManifestMetrics {
    pub fn_total: usize,
    pub fn_with_any_error: usize,
    pub fn_error_rate: f64,
}

impl SemanticManifestMetrics {
    pub fn score(&self) -> f64 {
        (1.0 - self.fn_error_rate).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SemanticSyncReport {
    pub metrics: SemanticManifestMetrics,
    pub rewrites_applied: bool,
}

#[derive(serde::Deserialize, Default)]
struct ProposalFile {
    #[serde(default)]
    fn_total: usize,
    #[serde(default)]
    fn_with_any_error: usize,
    #[serde(default)]
    fn_error_rate: f64,
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

pub fn load_semantic_manifest_metrics(workspace: &Path) -> SemanticManifestMetrics {
    let path = sidecar_path(workspace);
    let Ok(raw) = std::fs::read(&path) else {
        return SemanticManifestMetrics::default();
    };
    let Ok(file) = serde_json::from_slice::<ProposalFile>(&raw) else {
        return SemanticManifestMetrics::default();
    };
    SemanticManifestMetrics {
        fn_total: file.fn_total,
        fn_with_any_error: file.fn_with_any_error,
        fn_error_rate: file.fn_error_rate.clamp(0.0, 1.0),
    }
}

pub fn run_semantic_sync(workspace: &Path) -> Result<SemanticSyncReport, anyhow::Error> {
    let graph = graph_path(workspace);
    let sidecar = sidecar_path(workspace);
    let max_error_rate = std::env::var("SEM_MAX_ERROR_RATE").ok();
    let apply_rewrites = std::env::var("CANON_SEMANTIC_REWRITE")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);

    if let Some(parent) = sidecar.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let max_error_rate_value = max_error_rate
        .as_deref()
        .and_then(|v| v.parse::<f64>().ok());
    crate::semantic_manifest::run_with_options(crate::semantic_manifest::SemanticManifestRunOptions {
        workspace: workspace.to_path_buf(),
        graph_path: graph.clone(),
        out_path: sidecar.clone(),
        write_mode: true,
        max_error_rate: max_error_rate_value,
    })?;

    if apply_rewrites {
        crate::syn_writer::run_with_options(crate::syn_writer::SynWriterRunOptions {
            workspace_root: workspace.to_path_buf(),
            graph_path: graph.clone(),
            manifest_path: sidecar.clone(),
            log_path: workspace
                .join("agent_state")
                .join("syn_writer_log.json"),
            write_mode: true,
            augment: false,
            rewrite_existing: true,
        })?;

        // Refresh sidecar after rewrite.
        crate::semantic_manifest::run_with_options(crate::semantic_manifest::SemanticManifestRunOptions {
            workspace: workspace.to_path_buf(),
            graph_path: graph.clone(),
            out_path: sidecar.clone(),
            write_mode: true,
            max_error_rate: max_error_rate_value,
        })?;
    }

    Ok(SemanticSyncReport {
        metrics: load_semantic_manifest_metrics(workspace),
        rewrites_applied: apply_rewrites,
    })
}
