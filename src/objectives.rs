use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObjectivesFile {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub objectives: Vec<Objective>,
    #[serde(default)]
    pub goal: Vec<Value>,
    #[serde(default)]
    pub instrumentation: Vec<Value>,
    #[serde(default)]
    pub definition_of_done: Vec<Value>,
    #[serde(default)]
    pub non_goals: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Objective {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub authority_files: Vec<String>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub requirement: Vec<Value>,
    #[serde(default)]
    pub verification: Vec<Value>,
    #[serde(default)]
    pub success_criteria: Vec<Value>,
}

/// Compact one-liner-per-objective for prompt injection.
/// Strips description/requirement/verification/success_criteria to keep token cost low.
/// Only non-done objectives are included.
pub fn read_objectives_compact(path: &Path) -> String {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    if raw.trim().is_empty() {
        return String::new();
    }
    let Ok(file) = serde_json::from_str::<ObjectivesFile>(&raw) else {
        return raw;
    };
    let active: Vec<&Objective> = file
        .objectives
        .iter()
        .filter(|o| !is_completed(o))
        .collect();
    if active.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    // Limit to top-N objectives to prevent prompt overflow
    let limit = 20usize;
    for obj in active.iter().take(limit) {
        let status = if obj.status.trim().is_empty() {
            "active"
        } else {
            obj.status.trim()
        };
        let scope = if obj.scope.trim().is_empty() {
            String::new()
        } else {
            format!("  ({})", obj.scope.trim())
        };
        // Truncate overly long titles to prevent prompt overflow
        let max_len = 120usize;
        let title = obj.title.trim();
        let truncated = if title.len() > max_len {
            format!("{}…", &title[..max_len])
        } else {
            title.to_string()
        };
        out.push_str(&format!(
            "[{status}]  {}  —  {}{scope}\n",
            obj.id, truncated
        ));
    }
    out.push_str("Full detail: {\"action\":\"objectives\",\"op\":\"read\"}");
    out
}

pub fn filter_incomplete_objectives_json(raw: &str) -> Option<String> {
    let mut file: ObjectivesFile = serde_json::from_str(raw).ok()?;
    file.objectives = file
        .objectives
        .into_iter()
        .filter(|obj| !is_completed(obj))
        .collect();
    serde_json::to_string_pretty(&file).ok()
}

pub fn is_completed(obj: &Objective) -> bool {
    let status = if !obj.status.trim().is_empty() {
        Some(obj.status.trim().to_lowercase())
    } else {
        extract_status(&obj.description)
    };
    matches!(status.as_deref(), Some("done" | "complete" | "completed"))
}

pub fn extract_status(description: &str) -> Option<String> {
    let lower = description.to_lowercase();
    let marker = "status:";
    let idx = lower.find(marker)?;
    let rest = &lower[idx + marker.len()..];
    let rest = rest.trim_start();
    let rest = rest.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let end = rest
        .find("**")
        .or_else(|| rest.find('\n'))
        .unwrap_or_else(|| rest.len());
    let segment = rest[..end].trim();
    let status = segment.split_whitespace().next()?;
    Some(
        status
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_lowercase(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_status_parses_marked_status() {
        let desc = "**Status:** ready **Scope:** foo";
        assert_eq!(extract_status(desc).as_deref(), Some("ready"));
    }

    #[test]
    fn filter_incomplete_keeps_non_done() {
        let raw = r#"{
  "version": 1,
  "objectives": [
    {"id":"a","title":"A","category":"other","level":"low","description":"**Status:** done **Scope:** x","requirement":[],"verification":[],"success_criteria":[]},
    {"id":"b","title":"B","category":"other","level":"low","description":"**Status:** active **Scope:** y","requirement":[],"verification":[],"success_criteria":[]}
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#;
        let filtered = filter_incomplete_objectives_json(raw).unwrap();
        assert!(filtered.contains("\"id\": \"b\""));
        assert!(!filtered.contains("\"id\": \"a\""));
    }
}
