use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::semantic::SymbolSummary;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FingerprintDrift {
    pub improved: Vec<String>,
    pub regressed: Vec<String>,
    pub stable: Vec<String>,
    pub reward: f64,
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::path::Path, &[semantic::SymbolSummary], &[semantic::SymbolSummary]
/// Outputs: drift_analysis::FingerprintDrift
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn compute_fingerprint_drift(
    _workspace: &Path,
    prev_summaries: &[SymbolSummary],
    curr_summaries: &[SymbolSummary],
) -> FingerprintDrift {
    let prev: HashMap<&str, &SymbolSummary> = prev_summaries
        .iter()
        .map(|s| (s.symbol.as_str(), s))
        .collect();

    let mut improved = Vec::new();
    let mut regressed = Vec::new();
    let mut stable = Vec::new();

    for curr in curr_summaries {
        let Some(prev) = prev.get(curr.symbol.as_str()) else {
            continue;
        };
        if prev.mir_fingerprint == curr.mir_fingerprint {
            stable.push(curr.symbol.clone());
            continue;
        }
        let prev_score = complexity_score(prev);
        let curr_score = complexity_score(curr);
        if curr_score < prev_score {
            improved.push(curr.symbol.clone());
        } else if curr_score > prev_score {
            regressed.push(curr.symbol.clone());
        } else {
            stable.push(curr.symbol.clone());
        }
    }

    improved.sort();
    regressed.sort();
    stable.sort();

    let reward = improved.len() as f64 - 1.5 * regressed.len() as f64;

    FingerprintDrift {
        improved,
        regressed,
        stable,
        reward,
    }
}

fn complexity_score(summary: &SymbolSummary) -> f64 {
    let blocks = summary.mir_blocks.unwrap_or(0) as f64;
    let stmts = summary.mir_stmts.unwrap_or(0) as f64;
    let branch = summary.branch_score.unwrap_or(0.0);
    branch + blocks + (stmts / 10.0)
}
