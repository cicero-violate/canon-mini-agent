use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::semantic::SymbolSummary;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FingerprintDrift {
    pub improved: Vec<String>,
    pub regressed: Vec<String>,
    /// Backward-compatible legacy field.
    ///
    /// The tlog must not carry the full stable-symbol set on every complexity run.
    /// Use `stable_count` + `stable_hash` as the canonical compact metadata.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stable: Vec<String>,
    pub stable_count: usize,
    pub stable_hash: String,
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
    let mut stable_symbols = Vec::new();

    for curr in curr_summaries {
        let Some(prev) = prev.get(curr.symbol.as_str()) else {
            continue;
        };
        if prev.mir_fingerprint == curr.mir_fingerprint {
            stable_symbols.push(curr.symbol.clone());
            continue;
        }
        let prev_score = complexity_score(prev);
        let curr_score = complexity_score(curr);
        if curr_score < prev_score {
            improved.push(curr.symbol.clone());
        } else if curr_score > prev_score {
            regressed.push(curr.symbol.clone());
        } else {
            stable_symbols.push(curr.symbol.clone());
        }
    }

    improved.sort();
    regressed.sort();
    stable_symbols.sort();

    let reward = improved.len() as f64 - 1.5 * regressed.len() as f64;
    let stable_count = stable_symbols.len();
    let stable_hash = stable_symbols_hash(&stable_symbols);

    FingerprintDrift {
        improved,
        regressed,
        stable: Vec::new(),
        stable_count,
        stable_hash,
        reward,
    }
}

fn complexity_score(summary: &SymbolSummary) -> f64 {
    let blocks = summary.mir_blocks.unwrap_or(0) as f64;
    let stmts = summary.mir_stmts.unwrap_or(0) as f64;
    let branch = summary.branch_score.unwrap_or(0.0);
    branch + blocks + (stmts / 10.0)
}

fn stable_symbols_hash(symbols: &[String]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for symbol in symbols {
        for byte in symbol.as_bytes().iter().copied().chain([b'\n']) {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(symbol: &str, fingerprint: &str, branch_score: f64) -> SymbolSummary {
        SymbolSummary {
            symbol: symbol.to_string(),
            kind: "fn".to_string(),
            file: "src/lib.rs".to_string(),
            line: 1,
            signature: None,
            mir_fingerprint: Some(fingerprint.to_string()),
            mir_blocks: Some(1),
            mir_stmts: Some(1),
            call_in: 0,
            call_out: 0,
            ref_count: 0,
            branch_score: Some(branch_score),
            is_directly_recursive: false,
            assert_count: 0,
            drop_count: 0,
            switchint_count: 0,
            has_back_edges: false,
            clone_call_count: 0,
        }
    }

    #[test]
    fn stable_symbols_are_compacted_for_tlog_payloads() {
        let prev = vec![summary("a", "same", 1.0), summary("b", "same", 2.0)];
        let curr = vec![summary("b", "same", 2.0), summary("a", "same", 1.0)];

        let drift = compute_fingerprint_drift(Path::new("."), &prev, &curr);
        let payload = serde_json::to_string(&drift).expect("serialize drift");

        assert_eq!(drift.stable_count, 2);
        assert_eq!(drift.stable.len(), 0);
        assert!(!drift.stable_hash.is_empty());
        assert!(!payload.contains("\"stable\""));
        assert!(payload.contains("\"stable_count\":2"));
        assert!(payload.contains("\"stable_hash\""));
    }
}
