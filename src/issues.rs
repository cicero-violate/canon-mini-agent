use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::constants::ISSUES_FILE;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct IssuesFile {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub issues: Vec<Issue>,
}

/// A single issue recorded by any agent for later attention.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct Issue {
    /// Unique identifier, e.g. "ISS-001".
    pub id: String,
    /// Short human-readable title.
    pub title: String,
    /// open | in_progress | resolved | wontfix
    pub status: String,
    /// high | medium | low
    pub priority: String,
    /// Normalized priority score in [0.0, 1.0]. Auto-computed; do not set manually.
    /// Combines severity, recurrence, hot-path heuristic, and loop-velocity impact.
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub score: f32,
    /// bug | logic | invariant_violation | performance | stale_state
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// Full description of the issue and its impact.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// File path and/or component where the issue lives, e.g. "src/app.rs:420".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
    /// Concrete evidence strings (log lines, test failures, frame data, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// Agent role that discovered this issue, e.g. "solo" or "diagnostics".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub discovered_by: String,
}

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}

pub fn is_closed(issue: &Issue) -> bool {
    matches!(
        issue.status.trim().to_lowercase().as_str(),
        "resolved" | "wontfix" | "done" | "complete" | "completed" | "verified" | "closed"
    )
}

/// Compute a normalized [0.0, 1.0] priority score for an issue.
///
/// Weights:
///   severity      0.20 — priority string mapped to float
///   recurrence    0.20 — sibling issues with the same ID prefix (saturates at 3)
///   hot_path      0.25 — location/title mentions a per-turn code path
///   loop_velocity 0.35 — how much fixing this speeds up the agent's issue-close rate
pub fn compute_issue_score(issue: &Issue, all_issues: &[Issue]) -> f32 {
    // Severity from priority string
    let severity: f32 = match issue.priority.trim().to_lowercase().as_str() {
        "critical" => 1.0,
        "high" => 0.75,
        "medium" => 0.5,
        "low" => 0.25,
        _ => 0.5,
    };

    // Recurrence: count other issues that share the same leading token in their ID.
    // e.g. "ISS-DUPLICATE-1" and "ISS-DUPLICATE-2" are siblings.
    let base = issue.id.split('-').next().unwrap_or(&issue.id);
    let sibling_count = all_issues
        .iter()
        .filter(|i| i.id != issue.id && i.id.starts_with(base))
        .count();
    let recurrence = (sibling_count as f32 / 3.0).min(1.0);

    // Hot-path heuristic: is this in code that executes every agent turn?
    let combined = format!(
        "{} {} {}",
        issue.title.to_lowercase(),
        issue.description.to_lowercase(),
        issue.location.to_lowercase()
    );
    let hot_path_keywords = [
        "predicted_next_actions",
        "handle_batch",
        "canon-step",
        "canon_step",
        "every turn",
        "every cycle",
        "state_space",
        "dispatch",
    ];
    let hot_path: f32 = if hot_path_keywords.iter().any(|kw| combined.contains(kw)) {
        1.0
    } else {
        0.0
    };

    // Loop-velocity: how much does fixing this unblock the agent's self-improvement loop?
    let velocity: f32 = match issue.kind.trim().to_lowercase().as_str() {
        "bug" | "invariant_violation" => 1.0,
        "stale_state" | "logic" => 0.65,
        "performance" => 0.5,
        _ => {
            if issue.id.starts_with("auto_branch_reduce") || issue.id.starts_with("auto_refactor") {
                0.25
            } else {
                0.4
            }
        }
    };

    let score = 0.20 * severity + 0.20 * recurrence + 0.25 * hot_path + 0.35 * velocity;
    score.clamp(0.0, 1.0)
}

/// Recompute scores for every issue in the file.
/// Call this before writing so stored scores stay consistent.
pub fn rescore_all(file: &mut IssuesFile) {
    let snapshot = file.issues.clone();
    for issue in &mut file.issues {
        issue.score = compute_issue_score(issue, &snapshot);
    }
}

/// Read ISSUES.json and return the text of open/in-progress issues only.
/// Returns "(no open issues)" when the file is absent or all issues are closed.
pub fn read_open_issues(workspace: &Path) -> String {
    let path = workspace.join(ISSUES_FILE);
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    if raw.trim().is_empty() {
        return "(no open issues)".to_string();
    }
    let Ok(mut file) = serde_json::from_str::<IssuesFile>(&raw) else {
        return "(ISSUES.json is not valid JSON)".to_string();
    };
    file.issues.retain(|i| !is_closed(i));
    if file.issues.is_empty() {
        return "(no open issues)".to_string();
    }
    // Sort: high → medium → low, then by id.
    let priority_rank = |p: &str| match p.trim().to_lowercase().as_str() {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    };
    file.issues.sort_by(|a, b| {
        priority_rank(&a.priority)
            .cmp(&priority_rank(&b.priority))
            .then_with(|| a.id.cmp(&b.id))
    });
    serde_json::to_string_pretty(&file).unwrap_or(raw)
}

/// Read ISSUES.json and return a small human-readable summary of the top open issues.
/// Used for system-prompt priming; keep it short.
pub fn read_top_open_issues(workspace: &Path, limit: usize) -> String {
    let path = workspace.join(ISSUES_FILE);
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    if raw.trim().is_empty() {
        return "(no open issues)".to_string();
    }
    let Ok(mut file) = serde_json::from_str::<IssuesFile>(&raw) else {
        return "(ISSUES.json is not valid JSON)".to_string();
    };
    file.issues.retain(|i| !is_closed(i));
    if file.issues.is_empty() {
        return "(no open issues)".to_string();
    }
    let priority_rank = |p: &str| match p.trim().to_lowercase().as_str() {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    };
    file.issues.sort_by(|a, b| {
        priority_rank(&a.priority)
            .cmp(&priority_rank(&b.priority))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut out = String::new();
    out.push_str("Top open issues:\n");
    for issue in file.issues.into_iter().take(limit.max(1)) {
        let loc = if issue.location.trim().is_empty() {
            String::new()
        } else {
            format!(" ({})", issue.location.trim())
        };
        out.push_str(&format!(
            "- [{}] {}: {}{}\n",
            issue.priority.trim(),
            issue.id,
            issue.title.trim(),
            loc
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{is_closed, read_open_issues, read_top_open_issues, Issue};

    #[test]
    fn is_closed_treats_done_like_statuses_as_closed() {
        for status in [
            "resolved",
            "wontfix",
            "done",
            "complete",
            "completed",
            "verified",
            "closed",
        ] {
            let issue = Issue {
                status: status.to_string(),
                ..Issue::default()
            };
            assert!(is_closed(&issue), "status should be closed: {status}");
        }
    }

    #[test]
    fn read_open_issues_filters_done_entries() {
        let root = std::env::temp_dir().join(format!(
            "canon-mini-agent-issues-test-{}",
            crate::logging::now_ms()
        ));
        std::fs::create_dir_all(&root).expect("create temp issues dir");
        let path = root.join(crate::constants::ISSUES_FILE);
        let raw = r#"{
  "version": 0,
  "issues": [
    { "id": "i_done", "title": "done issue", "status": "done", "priority": "high" },
    { "id": "i_open", "title": "open issue", "status": "open", "priority": "high" }
  ]
}"#;
        std::fs::write(&path, raw).expect("write issues file");
        let filtered = read_open_issues(&root);
        assert!(filtered.contains("\"id\": \"i_open\""));
        assert!(!filtered.contains("\"id\": \"i_done\""));
    }

    #[test]
    fn read_top_open_issues_returns_small_summary() {
        let root = std::env::temp_dir().join(format!(
            "canon-mini-agent-issues-top-test-{}",
            crate::logging::now_ms()
        ));
        std::fs::create_dir_all(&root).expect("create temp issues dir");
        let path = root.join(crate::constants::ISSUES_FILE);
        let raw = r#"{
  "version": 0,
  "issues": [
    { "id": "i_low", "title": "low issue", "status": "open", "priority": "low", "location": "a.rs:1" },
    { "id": "i_high", "title": "high issue", "status": "open", "priority": "high", "location": "b.rs:2" }
  ]
}"#;
        std::fs::write(&path, raw).expect("write issues file");
        let summary = read_top_open_issues(&root, 1);
        assert!(summary.contains("Top open issues"));
        assert!(summary.contains("i_high"));
        assert!(!summary.contains("i_low"));
    }
}
