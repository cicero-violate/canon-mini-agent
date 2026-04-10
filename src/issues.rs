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

pub fn is_closed(issue: &Issue) -> bool {
    matches!(
        issue.status.trim().to_lowercase().as_str(),
        "resolved" | "wontfix" | "done" | "complete" | "completed" | "verified" | "closed"
    )
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

#[cfg(test)]
mod tests {
    use super::{is_closed, read_open_issues, Issue};

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
}
