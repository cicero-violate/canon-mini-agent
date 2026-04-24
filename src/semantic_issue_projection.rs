use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;

use crate::issues::{load_issues_file, persist_issues_projection_with_writer, rescore_all, Issue, IssuesFile};

const ISSUE_ID_PREFIX: &str = "auto_semantic_rank_candidate_";
const DISCOVERED_BY: &str = "semantic_rank_projection";

#[derive(Debug, Clone, Default)]
pub struct SemanticIssueProjectionReport {
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

#[derive(Debug, Deserialize, Clone)]
struct RankCandidate {
    #[serde(default)]
    rank: usize,
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
    #[serde(default)]
    reasoning: Vec<String>,
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
        candidate.owner,
        candidate.owner_node_id,
        candidate.rank
    );
    let hash = crate::logging::stable_hash_hex(&key);
    format!("{ISSUE_ID_PREFIX}{}", sanitize_fragment(&hash))
}

fn action_tier(action: &str) -> u8 {
    match action {
        "safe_merge" => 0,
        "investigate" => 1,
        _ => 2,
    }
}

fn rank_signal(candidate: &RankCandidate) -> f64 {
    let pair_bonus = ((candidate.pair_count as f64).log2() / 6.0).clamp(0.0, 1.0);
    (0.7 * candidate.confidence + 0.3 * pair_bonus).clamp(0.0, 1.0)
}

fn priority_for(candidate: &RankCandidate) -> &'static str {
    let signal = rank_signal(candidate);
    if candidate.recommended_action == "safe_merge" {
        if signal >= 0.85 {
            "high"
        } else {
            "medium"
        }
    } else if signal >= 0.65 {
        "medium"
    } else {
        "low"
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
    let effects = if candidate.effects.is_empty() {
        "unknown".to_string()
    } else {
        candidate.effects.join(", ")
    };
    let reasoning_preview = if candidate.reasoning.is_empty() {
        "none".to_string()
    } else {
        candidate.reasoning.iter().take(5).cloned().collect::<Vec<_>>().join("; ")
    };
    let mut evidence = vec![
        format!("owner={}", candidate.owner),
        format!("owner_node_id={}", candidate.owner_node_id),
        format!("rank={}", candidate.rank),
        format!("confidence={:.2}", candidate.confidence),
        format!("pair_count={}", candidate.pair_count),
        format!("recommended_action={}", candidate.recommended_action),
    ];
    if !candidate.reasoning.is_empty() {
        evidence.push(format!("reasoning={reasoning_preview}"));
    }
    Issue {
        id: issue_id(candidate),
        title,
        status: "open".to_string(),
        priority: priority_for(candidate).to_string(),
        kind: "redundancy".to_string(),
        description: format!(
            "Semantic rank projected this redundant-path candidate as `{}`.\n\
             owner: `{}`\n\
             confidence: {:.2}\n\
             pair_count: {}\n\
             intent: `{}` resource: `{}` effects: {}\n\n\
             This issue is auto-generated from `safe_patch_candidates.json` and should map to a \
             concrete merge/delete refactor task with verification.",
            candidate.recommended_action,
            candidate.owner,
            candidate.confidence,
            candidate.pair_count,
            intent,
            resource,
            effects
        ),
        location: "state/rustc/canon_mini_agent/graph.json".to_string(),
        scope: "crate:canon_mini_agent".to_string(),
        metrics: json!({
            "task": "FoldRedundantPath",
            "proof_tier": "hypothesis",
            "source": "safe_patch_candidates.json",
            "owner": candidate.owner,
            "owner_node_id": candidate.owner_node_id,
            "rank": candidate.rank,
            "confidence": candidate.confidence,
            "pair_count": candidate.pair_count,
            "recommended_action": candidate.recommended_action,
            "intent_class": candidate.intent_class,
            "resource": candidate.resource,
            "effects": candidate.effects,
            // Integrates with global issue scoring detector_signal path.
            "redundancy_ratio": rank_signal(candidate),
        }),
        acceptance_criteria: vec![
            "Chosen candidate is refactored into a single canonical implementation path".to_string(),
            "cargo build/test passes after the refactor".to_string(),
            "After graph regeneration, candidate no longer appears in active semantic rank output".to_string(),
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

fn resolve_missing_semantic_issues(file: &mut IssuesFile, active_ids: &HashSet<String>) -> usize {
    let mut resolved = 0usize;
    for issue in &mut file.issues {
        if !issue.id.starts_with(ISSUE_ID_PREFIX) {
            continue;
        }
        if active_ids.contains(&issue.id) {
            continue;
        }
        if crate::issues::is_closed(issue) {
            continue;
        }
        issue.status = "resolved".to_string();
        resolved += 1;
    }
    resolved
}

fn load_rank_file(path: &Path) -> Result<RankFile> {
    let bytes = std::fs::read(path)?;
    let parsed: RankFile = serde_json::from_slice(&bytes)?;
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

fn selected_candidates(mut all: Vec<RankCandidate>) -> Vec<RankCandidate> {
    all.retain(|c| c.recommended_action == "safe_merge" || c.recommended_action == "investigate");
    all.sort_by(|a, b| {
        action_tier(&a.recommended_action)
            .cmp(&action_tier(&b.recommended_action))
            .then(b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal))
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
    let selected = selected_candidates(rank.candidates);
    let mut file = load_issues_file(workspace);
    let mut opened_or_updated = 0usize;
    let mut active_ids: HashSet<String> = HashSet::new();
    for candidate in &selected {
        let issue = build_issue(candidate);
        active_ids.insert(issue.id.clone());
        if upsert_issue(&mut file, issue) {
            opened_or_updated += 1;
        }
    }
    let resolved_stale = resolve_missing_semantic_issues(&mut file, &active_ids);
    let rewrote_issues = opened_or_updated > 0 || resolved_stale > 0;
    if rewrote_issues {
        rescore_all(&mut file);
        persist_issues_projection_with_writer(workspace, &file, None, "semantic_rank_issue_projection")?;
    }
    Ok(SemanticIssueProjectionReport {
        selected_candidates: active_ids.len(),
        opened_or_updated,
        resolved_stale,
        rewrote_issues,
    })
}

pub fn project_from_cli_args(
    args: &[String],
    workspace: &Path,
) -> Result<SemanticIssueProjectionReport> {
    let rank_path = args
        .first()
        .map(|s| workspace.join(s))
        .unwrap_or_else(|| workspace.join("agent_state").join("safe_patch_candidates.json"));
    project_semantic_rank_issues(workspace, rank_path.as_path())
}
