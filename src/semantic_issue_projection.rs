use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::issues::{
    load_issues_file, persist_issues_projection_with_writer, rescore_all, Issue, IssuesFile,
};

const ISSUE_ID_PREFIX: &str = "auto_semantic_rank_candidate_";
const DISCOVERED_BY: &str = "semantic_rank_projection";
const MANIFEST_ISSUE_ID_PREFIX: &str = "auto_semantic_manifest_error_";
const MANIFEST_DISCOVERED_BY: &str = "semantic_manifest_projection";

#[derive(Debug, Clone, Default)]
pub struct SemanticIssueProjectionReport {
    pub rank_selected_candidates: usize,
    pub manifest_selected_candidates: usize,
    pub selected_candidates: usize,
    pub opened_or_updated: usize,
    pub resolved_stale: usize,
    pub rewrote_issues: bool,
}

#[derive(Debug, Deserialize)]
struct RankFile {
    #[serde(default)]
    candidates: Vec<RankCandidate>,
}

#[derive(Debug, Deserialize)]
struct ManifestProposalFile {
    #[serde(default)]
    proposals: HashMap<String, ManifestProposal>,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct ManifestProposal {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    file: String,
    #[serde(default)]
    line: String,
    #[serde(default)]
    intent_class: String,
    #[serde(default)]
    resource: String,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    effects: Vec<String>,
    #[serde(default)]
    forbidden_effects: Vec<String>,
    #[serde(default)]
    failure_mode: String,
    #[serde(default)]
    invariants: Vec<String>,
    #[serde(default)]
    provenance: Vec<String>,
    #[serde(default)]
    manifest_status: String,
}

#[derive(Debug, Deserialize, Clone)]
struct RankCandidate {
    #[serde(default)]
    owner: String,
    #[serde(default)]
    owner_node_id: String,
    #[serde(default)]
    pair_count: usize,
    #[serde(default)]
    confidence: f64,
    #[serde(default)]
    recommended_action: String,
    #[serde(default)]
    intent_class: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    effects: Vec<String>,
}

fn sanitize_fragment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

fn issue_id(candidate: &RankCandidate) -> String {
    let key = format!(
        "{}|{}|{}",
        candidate.owner, candidate.owner_node_id, candidate.recommended_action
    );
    let hash = crate::logging::stable_hash_hex(&key);
    format!("{ISSUE_ID_PREFIX}{}", sanitize_fragment(&hash))
}

fn manifest_issue_id(node_id: &str) -> String {
    let hash = crate::logging::stable_hash_hex(node_id);
    format!("{MANIFEST_ISSUE_ID_PREFIX}{}", sanitize_fragment(&hash))
}

fn action_tier(action: &str) -> u8 {
    match action {
        "safe_merge" => 0,
        "investigate" => 1,
        _ => 2,
    }
}

fn priority_for(candidate: &RankCandidate) -> &'static str {
    if candidate.recommended_action == "safe_merge" {
        "high"
    } else if candidate.recommended_action == "investigate" {
        "medium"
    } else {
        "low"
    }
}

fn action_signal(action: &str) -> f64 {
    match action {
        "safe_merge" => 1.0,
        "investigate" => 0.5,
        _ => 0.25,
    }
}

fn build_issue(candidate: &RankCandidate) -> Issue {
    let title = match candidate.recommended_action.as_str() {
        "safe_merge" => format!(
            "Apply semantic redundancy merge candidate: {}",
            candidate.owner
        ),
        _ => format!(
            "Investigate semantic redundancy candidate: {}",
            candidate.owner
        ),
    };
    let intent = candidate.intent_class.as_deref().unwrap_or("unknown");
    let resource = candidate.resource.as_deref().unwrap_or("unknown");
    let mut effect_values = candidate.effects.clone();
    effect_values.sort();
    effect_values.dedup();
    let effects = if effect_values.is_empty() {
        "unknown".to_string()
    } else {
        effect_values.join(", ")
    };
    let mut evidence = vec![
        format!("owner={}", candidate.owner),
        format!("owner_node_id={}", candidate.owner_node_id),
        format!("recommended_action={}", candidate.recommended_action),
    ];
    evidence.sort();
    Issue {
        id: issue_id(candidate),
        title,
        status: "open".to_string(),
        priority: priority_for(candidate).to_string(),
        kind: "redundancy".to_string(),
        description: format!(
            "Semantic rank projected this redundant-path candidate as `{}`.\n\
             owner: `{}`\n\
             intent: `{}` resource: `{}` effects: {}\n\n\
             This issue is auto-generated from `safe_patch_candidates.json` and should map to a \
             concrete merge/delete refactor task with verification.",
            candidate.recommended_action, candidate.owner, intent, resource, effects
        ),
        location: "state/rustc/canon_mini_agent/graph.json".to_string(),
        scope: "crate:canon_mini_agent".to_string(),
        metrics: json!({
            "task": "FoldRedundantPath",
            "proof_tier": "hypothesis",
            "source": "safe_patch_candidates.json",
            "owner": candidate.owner,
            "owner_node_id": candidate.owner_node_id,
            "recommended_action": candidate.recommended_action,
            "intent_class": candidate.intent_class,
            "resource": candidate.resource,
            "effects": effect_values,
            // Integrates with global issue scoring detector_signal path.
            // This intentionally uses the stable action tier, not rank/confidence/pair_count.
            // Rank-derived values churn every graph rebuild and previously caused repeated
            // 25-37MB ISSUES.json projection rewrites.
            "redundancy_ratio": action_signal(&candidate.recommended_action),
        }),
        acceptance_criteria: vec![
            "Chosen candidate is refactored into a single canonical implementation path"
                .to_string(),
            "cargo build/test passes after the refactor".to_string(),
            "After graph regeneration, candidate no longer appears in active semantic rank output"
                .to_string(),
        ],
        evidence,
        discovered_by: DISCOVERED_BY.to_string(),
        ..Issue::default()
    }
}

fn upsert_issue(file: &mut IssuesFile, desired: Issue) -> bool {
    if let Some(existing) = file.issues.iter_mut().find(|issue| issue.id == desired.id) {
        let mut changed = false;
        macro_rules! sync_field {
            ($field:ident) => {
                if existing.$field != desired.$field {
                    existing.$field = desired.$field.clone();
                    changed = true;
                }
            };
        }
        sync_field!(title);
        if existing.status != "open" {
            existing.status = "open".to_string();
            changed = true;
        }
        sync_field!(priority);
        sync_field!(kind);
        sync_field!(description);
        sync_field!(location);
        sync_field!(metrics);
        sync_field!(scope);
        sync_field!(acceptance_criteria);
        sync_field!(evidence);
        sync_field!(discovered_by);
        changed
    } else {
        file.issues.push(desired);
        true
    }
}

fn contains_error_scalar(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.is_empty() || trimmed == "error" || trimmed.eq_ignore_ascii_case("todo")
}

fn contains_error_list(values: &[String]) -> bool {
    values.iter().any(|v| contains_error_scalar(v))
}

fn manifest_error_fields(manifest: &ManifestProposal) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if contains_error_scalar(&manifest.intent_class) {
        fields.push("intent_class");
    }
    if contains_error_scalar(&manifest.resource) {
        fields.push("resource");
    }
    if contains_error_list(&manifest.inputs) {
        fields.push("inputs");
    }
    if contains_error_list(&manifest.outputs) {
        fields.push("outputs");
    }
    if contains_error_list(&manifest.effects) {
        fields.push("effects");
    }
    if contains_error_list(&manifest.forbidden_effects) {
        fields.push("forbidden_effects");
    }
    if contains_error_scalar(&manifest.failure_mode) {
        fields.push("failure_mode");
    }
    if contains_error_list(&manifest.invariants) {
        fields.push("invariants");
    }
    if contains_error_list(&manifest.provenance) {
        fields.push("provenance");
    }
    fields
}

fn manifest_error_priority(error_fields: usize) -> &'static str {
    if error_fields >= 4 {
        "high"
    } else if error_fields >= 2 {
        "medium"
    } else {
        "low"
    }
}

fn manifest_error_location(manifest: &ManifestProposal) -> String {
    let file = manifest.file.trim();
    let line = manifest.line.trim();
    if !file.is_empty() && !line.is_empty() {
        format!("{file}:{line}")
    } else if !file.is_empty() {
        file.to_string()
    } else {
        "state/rustc/canon_mini_agent/graph.json".to_string()
    }
}

fn build_manifest_error_issue(
    node_id: &str,
    manifest: &ManifestProposal,
    error_fields: &[&str],
) -> Issue {
    let symbol = if manifest.symbol.trim().is_empty() {
        node_id
    } else {
        manifest.symbol.trim()
    };
    Issue {
        id: manifest_issue_id(node_id),
        title: format!("Repair semantic manifest contract errors: {symbol}"),
        status: "open".to_string(),
        priority: manifest_error_priority(error_fields.len()).to_string(),
        kind: "invariant_violation".to_string(),
        description: format!(
            "Semantic manifest contains unresolved contract placeholders for `{symbol}`.\n\
             error fields: {}\n\
             Fix the function docstring contract fields and regenerate semantic artifacts.",
            error_fields.join(", ")
        ),
        location: manifest_error_location(manifest),
        scope: "crate:canon_mini_agent".to_string(),
        metrics: json!({
            "task": "RepairSemanticManifestContract",
            "proof_tier": "deterministic",
            "source": "semantic_manifest_proposals.json",
            "node_id": node_id,
            "symbol": symbol,
            "manifest_status": manifest.manifest_status,
            "error_fields": error_fields,
            "error_field_count": error_fields.len(),
        }),
        acceptance_criteria: vec![
            "Function semantic_manifest has no `error` placeholders".to_string(),
            "semantic_manifest_proposals.json marks the function as `complete`".to_string(),
            "cargo check -p canon-mini-agent passes".to_string(),
        ],
        evidence: vec![
            format!("node_id={node_id}"),
            format!("symbol={symbol}"),
            format!("manifest_status={}", manifest.manifest_status),
            format!("error_fields={}", error_fields.join(",")),
        ],
        discovered_by: MANIFEST_DISCOVERED_BY.to_string(),
        ..Issue::default()
    }
}

fn selected_manifest_errors(
    proposals: HashMap<String, ManifestProposal>,
) -> Vec<(String, ManifestProposal, Vec<&'static str>)> {
    let mut out = Vec::new();
    for (node_id, manifest) in proposals {
        if manifest.kind != "fn" {
            continue;
        }
        let error_fields = manifest_error_fields(&manifest);
        if error_fields.is_empty() {
            continue;
        }
        out.push((node_id, manifest, error_fields));
    }
    out.sort_by(|a, b| {
        b.2.len()
            .cmp(&a.2.len())
            .then(a.1.symbol.cmp(&b.1.symbol))
            .then(a.0.cmp(&b.0))
    });
    out.truncate(manifest_selection_limit());
    out
}

fn prune_missing_projected_issues(
    file: &mut IssuesFile,
    active_ids: &HashSet<String>,
    id_prefix: &str,
) -> usize {
    let before = file.issues.len();
    file.issues
        .retain(|issue| !issue.id.starts_with(id_prefix) || active_ids.contains(&issue.id));
    before.saturating_sub(file.issues.len())
}

fn load_rank_file(path: &Path) -> Result<RankFile> {
    let bytes = std::fs::read(path)?;
    let parsed: RankFile = serde_json::from_slice(&bytes)?;
    Ok(parsed)
}

fn load_manifest_file(path: &Path) -> Result<ManifestProposalFile> {
    let bytes = std::fs::read(path)?;
    let parsed: ManifestProposalFile = serde_json::from_slice(&bytes)?;
    Ok(parsed)
}

fn selection_limits() -> (usize, usize) {
    let max_safe = std::env::var("SEM_ISSUE_MAX_SAFE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(64);
    let max_investigate = std::env::var("SEM_ISSUE_MAX_INVESTIGATE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(64);
    (max_safe.max(1), max_investigate.max(1))
}

fn manifest_selection_limit() -> usize {
    std::env::var("SEM_ISSUE_MAX_MANIFEST_ERRORS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(256)
        .max(1)
}

fn selected_candidates(mut all: Vec<RankCandidate>) -> Vec<RankCandidate> {
    all.retain(|c| c.recommended_action == "safe_merge" || c.recommended_action == "investigate");
    all.sort_by(|a, b| {
        action_tier(&a.recommended_action)
            .cmp(&action_tier(&b.recommended_action))
            .then(
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(b.pair_count.cmp(&a.pair_count))
            .then(a.owner.cmp(&b.owner))
    });
    let (max_safe, max_investigate) = selection_limits();
    let mut safe_used = 0usize;
    let mut inv_used = 0usize;
    let mut out = Vec::new();
    for c in all {
        if c.recommended_action == "safe_merge" {
            if safe_used >= max_safe {
                continue;
            }
            safe_used += 1;
            out.push(c);
        } else {
            if inv_used >= max_investigate {
                continue;
            }
            inv_used += 1;
            out.push(c);
        }
    }
    out
}

pub fn project_semantic_rank_issues(
    workspace: &Path,
    rank_path: &Path,
) -> Result<SemanticIssueProjectionReport> {
    let rank = load_rank_file(rank_path)?;
    let selected_rank = selected_candidates(rank.candidates);
    let manifest_path = workspace
        .join("agent_state")
        .join("semantic_manifest_proposals.json");
    let selected_manifest = if manifest_path.exists() {
        let file = load_manifest_file(manifest_path.as_path())?;
        selected_manifest_errors(file.proposals)
    } else {
        Vec::new()
    };
    let mut file = load_issues_file(workspace);
    let mut opened_or_updated = 0usize;
    let mut active_rank_ids: HashSet<String> = HashSet::new();
    for candidate in &selected_rank {
        let issue = build_issue(candidate);
        active_rank_ids.insert(issue.id.clone());
        if upsert_issue(&mut file, issue) {
            opened_or_updated += 1;
        }
    }
    let mut active_manifest_ids: HashSet<String> = HashSet::new();
    for (node_id, manifest, error_fields) in &selected_manifest {
        let issue = build_manifest_error_issue(node_id, manifest, error_fields);
        active_manifest_ids.insert(issue.id.clone());
        if upsert_issue(&mut file, issue) {
            opened_or_updated += 1;
        }
    }
    let resolved_stale =
        prune_missing_projected_issues(&mut file, &active_rank_ids, ISSUE_ID_PREFIX)
            + prune_missing_projected_issues(
                &mut file,
                &active_manifest_ids,
                MANIFEST_ISSUE_ID_PREFIX,
            );
    let selected_candidates = active_rank_ids.len() + active_manifest_ids.len();
    let rewrote_issues = opened_or_updated > 0 || resolved_stale > 0;
    if rewrote_issues {
        rescore_all(&mut file);
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            "semantic_rank_issue_projection",
        )?;
    }
    Ok(SemanticIssueProjectionReport {
        rank_selected_candidates: active_rank_ids.len(),
        manifest_selected_candidates: active_manifest_ids.len(),
        selected_candidates,
        opened_or_updated,
        resolved_stale,
        rewrote_issues,
    })
}

pub fn project_from_cli_args(
    args: &[String],
    workspace: &Path,
) -> Result<SemanticIssueProjectionReport> {
    let rank_path = args.first().map(|s| workspace.join(s)).unwrap_or_else(|| {
        workspace
            .join("agent_state")
            .join("safe_patch_candidates.json")
    });
    project_semantic_rank_issues(workspace, rank_path.as_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(_rank: usize, confidence: f64, pair_count: usize) -> RankCandidate {
        RankCandidate {
            owner: "canon_mini_agent::runtime::drive".to_string(),
            owner_node_id: "fn:drive".to_string(),
            pair_count,
            confidence,
            recommended_action: "safe_merge".to_string(),
            intent_class: Some("canonical_write".to_string()),
            resource: Some("state".to_string()),
            effects: vec!["logging".to_string()],
        }
    }

    #[test]
    fn semantic_rank_issue_identity_ignores_rank_noise() {
        let first = candidate(1, 0.98, 144);
        let second = candidate(99, 0.41, 3);

        let first_issue = build_issue(&first);
        let second_issue = build_issue(&second);

        assert_eq!(first_issue.id, second_issue.id);
        assert_eq!(first_issue.priority, second_issue.priority);
        assert_eq!(first_issue.description, second_issue.description);
        assert_eq!(first_issue.evidence, second_issue.evidence);
        assert_eq!(first_issue.metrics, second_issue.metrics);
    }

    #[test]
    fn missing_projected_issues_are_pruned_not_retained_as_closed_noise() {
        let active_id = format!("{ISSUE_ID_PREFIX}active");
        let stale_id = format!("{ISSUE_ID_PREFIX}stale");
        let manual_id = "ISS-MANUAL-1".to_string();
        let mut active_ids = HashSet::new();
        active_ids.insert(active_id.clone());
        let mut file = IssuesFile {
            version: 1,
            issues: vec![
                Issue {
                    id: active_id.clone(),
                    status: "open".to_string(),
                    ..Issue::default()
                },
                Issue {
                    id: stale_id.clone(),
                    status: "open".to_string(),
                    ..Issue::default()
                },
                Issue {
                    id: manual_id.clone(),
                    status: "open".to_string(),
                    ..Issue::default()
                },
            ],
        };

        let pruned = prune_missing_projected_issues(&mut file, &active_ids, ISSUE_ID_PREFIX);
        let remaining = file
            .issues
            .iter()
            .map(|issue| issue.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(pruned, 1);
        assert_eq!(remaining, vec![active_id.as_str(), manual_id.as_str()]);
        assert!(!remaining.contains(&stale_id.as_str()));
    }
}
