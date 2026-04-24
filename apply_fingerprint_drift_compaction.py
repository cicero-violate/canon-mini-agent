#!/usr/bin/env python3
from pathlib import Path
import difflib
import subprocess
import sys
import os

repo = Path.cwd()
path = repo / "src" / "drift_analysis.rs"
if not path.exists():
    raise SystemExit(f"missing {path}")

old = path.read_text()

if "pub stable_count: usize" in old and "pub stable_hash: String" in old and "stable: Vec::new()" in old:
    print("already applied: fingerprint drift stable payload is compacted")
    raise SystemExit(0)

new = old

# 1) Add compact metadata fields while keeping stable as a backward-compatible empty legacy field.
new = new.replace(
'''pub struct FingerprintDrift {
    pub improved: Vec<String>,
    pub regressed: Vec<String>,
    pub stable: Vec<String>,
    pub reward: f64,
}''',
'''pub struct FingerprintDrift {
    pub improved: Vec<String>,
    pub regressed: Vec<String>,
    /// Backward-compatible legacy field.
    ///
    /// The tlog must not carry the full stable-symbol set on every complexity run.
    /// Use `stable_count` + `stable_hash` as the canonical compact metadata.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stable: Vec<String>,
    #[serde(default)]
    pub stable_count: usize,
    #[serde(default)]
    pub stable_hash: String,
    pub reward: f64,
}'''
)

# 2) Rename local stable vector to avoid confusing it with serialized legacy field.
new = new.replace("let mut stable = Vec::new();", "let mut stable_symbols = Vec::new();")
new = new.replace("stable.push(curr.symbol.clone());", "stable_symbols.push(curr.symbol.clone());")
new = new.replace("stable.sort();", "stable_symbols.sort();")
new = new.replace(
'''    let reward = improved.len() as f64 - 1.5 * regressed.len() as f64;

    FingerprintDrift {
        improved,
        regressed,
        stable,
        reward,
    }''',
'''    let reward = improved.len() as f64 - 1.5 * regressed.len() as f64;
    let stable_count = stable_symbols.len();
    let stable_hash = stable_symbols_hash(&stable_symbols);

    FingerprintDrift {
        improved,
        regressed,
        stable: Vec::new(),
        stable_count,
        stable_hash,
        reward,
    }'''
)

# 3) Add stable hash helper if missing.
if "fn stable_symbols_hash(" not in new:
    marker = '''fn complexity_score(summary: &SymbolSummary) -> f64 {
    let blocks = summary.mir_blocks.unwrap_or(0) as f64;
    let stmts = summary.mir_stmts.unwrap_or(0) as f64;
    let branch = summary.branch_score.unwrap_or(0.0);
    branch + blocks + (stmts / 10.0)
}
'''
    helper = marker + r'''
fn stable_symbols_hash(symbols: &[String]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for symbol in symbols {
        for byte in symbol.as_bytes().iter().copied().chain([b'
']) {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}
'''
    new = new.replace(marker, helper)

# 4) Add regression test if missing.
if "stable_symbols_are_compacted_for_tlog_payloads" not in new:
    new += r'''
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
'''

if new == old:
    print("no changes generated; source shape is different than expected")
    sys.exit(1)

patch = ''.join(difflib.unified_diff(
    old.splitlines(True),
    new.splitlines(True),
    fromfile='a/src/drift_analysis.rs',
    tofile='b/src/drift_analysis.rs',
    n=3,
))

cmd = ["/opt/apply_patch/apply_patch_v3"]
if not Path(cmd[0]).exists():
    cmd = ["/opt/apply_patch/bin/apply_patch"]
env = {"PYTHONPATH": "/opt/pyvenv/lib/python3.13/site-packages", "PATH": os.environ.get("PATH", "/usr/bin")}
res = subprocess.run(cmd, input=patch, text=True, cwd=repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
print(res.stdout, end="")
print(res.stderr, end="", file=sys.stderr)
if res.returncode != 0:
    print(patch)
sys.exit(res.returncode)
