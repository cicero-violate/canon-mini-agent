use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const LEGACY_OBJECTIVES_JSON_FILE: &str = "PLANS/OBJECTIVES.json";

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

pub fn runtime_objectives_path(workspace: &Path) -> PathBuf {
    workspace_join_path(workspace, crate::constants::OBJECTIVES_FILE)
}

pub fn legacy_objectives_path(workspace: &Path) -> PathBuf {
    workspace_join_path(workspace, LEGACY_OBJECTIVES_JSON_FILE)
}

pub fn workspace_join_path(workspace: &Path, file: &str) -> PathBuf {
    workspace.join(file)
}

pub fn resolve_objectives_path(workspace: &Path) -> PathBuf {
    let runtime = runtime_objectives_path(workspace);
    if runtime.exists() {
        return runtime;
    }
    let legacy = legacy_objectives_path(workspace);
    if legacy.exists() {
        return legacy;
    }
    runtime
}

pub fn ensure_runtime_objectives_file(workspace: &Path) -> std::io::Result<PathBuf> {
    let runtime = runtime_objectives_path(workspace);
    if runtime.exists() {
        return Ok(runtime);
    }

    if let Some(parent) = runtime.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let legacy = legacy_objectives_path(workspace);
    if legacy.exists() {
        let raw = std::fs::read_to_string(&legacy).unwrap_or_default();
        if !raw.trim().is_empty() {
            std::fs::write(&runtime, raw)?;
            return Ok(runtime);
        }
    }

    let empty = serde_json::to_string_pretty(&ObjectivesFile {
        version: 1,
        ..ObjectivesFile::default()
    })
    .unwrap_or_else(|_| {
        "{\"version\":1,\"objectives\":[],\"goal\":[],\"instrumentation\":[],\"definition_of_done\":[],\"non_goals\":[]}".to_string()
    });
    std::fs::write(&runtime, empty)?;
    Ok(runtime)
}

pub fn read_objectives_compact_for_workspace(workspace: &Path) -> String {
    if let Some(canonical) = load_canonical_objectives_json(workspace) {
        return read_objectives_compact_from_raw(&canonical);
    }
    let path = resolve_objectives_path(workspace);
    read_objectives_compact(&path)
}

/// Compact one-liner-per-objective for prompt injection.
/// Strips description/requirement/verification/success_criteria to keep token cost low.
/// Only non-done objectives are included.
pub fn read_objectives_compact(path: &Path) -> String {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    read_objectives_compact_from_raw(&raw)
}

pub fn read_objectives_compact_from_raw(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let Ok(file) = serde_json::from_str::<ObjectivesFile>(&raw) else {
        return raw.to_string();
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

pub fn load_bootstrap_objectives_seed(workspace: &Path) -> (PathBuf, String) {
    let path = ensure_runtime_objectives_file(workspace)
        .unwrap_or_else(|_| resolve_objectives_path(workspace));
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let normalized = if raw.trim().is_empty() {
        serde_json::to_string_pretty(&ObjectivesFile {
            version: 1,
            ..ObjectivesFile::default()
        })
        .unwrap_or_else(|_| "{\"version\":1,\"objectives\":[]}".to_string())
    } else {
        raw
    };
    (path, normalized)
}

pub fn load_runtime_objectives_json(workspace: &Path) -> String {
    if let Some(canonical) = load_canonical_objectives_json(workspace) {
        return canonical;
    }
    let (_path, raw) = load_bootstrap_objectives_seed(workspace);
    raw
}

pub fn objectives_hash(raw: &str) -> String {
    crate::logging::stable_hash_hex(raw)
}

pub fn persist_objectives_projection(workspace: &Path, raw: &str, subject: &str) -> anyhow::Result<()> {
    crate::logging::write_projection_with_artifact_effects(
        workspace,
        &workspace.join(crate::constants::OBJECTIVES_FILE),
        crate::constants::OBJECTIVES_FILE,
        "write",
        subject,
        raw,
    )
}

pub fn reconcile_objectives_projection(workspace: &Path, canonical_raw: &str, subject: &str) -> anyhow::Result<bool> {
    let path = workspace.join(crate::constants::OBJECTIVES_FILE);
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if objectives_hash(&current) == objectives_hash(canonical_raw) {
        return Ok(false);
    }
    persist_objectives_projection(workspace, canonical_raw, subject)?;
    Ok(true)
}

pub fn load_canonical_objectives_json(workspace: &Path) -> Option<String> {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let replayed = crate::tlog::Tlog::replay_canonical_state(&tlog_path).ok()?;
    if replayed.objectives_json.trim().is_empty() {
        None
    } else {
        Some(replayed.objectives_json)
    }
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fresh_workspace(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "canon-mini-agent-objectives-{name}-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

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

    #[test]
    fn resolve_objectives_path_prefers_legacy_when_runtime_missing() {
        let workspace = fresh_workspace("legacy-fallback");
        let legacy = legacy_objectives_path(&workspace);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, r#"{"version":1,"objectives":[{"id":"obj_legacy","title":"Legacy","status":"active"}],"goal":[],"instrumentation":[],"definition_of_done":[],"non_goals":[]}"#).unwrap();

        assert_eq!(resolve_objectives_path(&workspace), legacy);
        assert!(read_objectives_compact_for_workspace(&workspace).contains("obj_legacy"));
    }

    #[test]
    fn ensure_runtime_objectives_file_bootstraps_from_legacy_json() {
        let workspace = fresh_workspace("bootstrap-runtime");
        let legacy = legacy_objectives_path(&workspace);
        let runtime = runtime_objectives_path(&workspace);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, r#"{"version":1,"objectives":[{"id":"obj_runtime","title":"Restore runtime objective authority","status":"active"}],"goal":[],"instrumentation":[],"definition_of_done":[],"non_goals":[]}"#).unwrap();

        let ensured = ensure_runtime_objectives_file(&workspace).unwrap();
        let persisted = std::fs::read_to_string(&runtime).unwrap();

        assert_eq!(ensured, runtime);
        assert!(persisted.contains("obj_runtime"));
    }

    #[test]
    fn ensure_runtime_objectives_file_creates_empty_runtime_authority_when_absent() {
        let workspace = fresh_workspace("bootstrap-empty");
        let runtime = ensure_runtime_objectives_file(&workspace).unwrap();
        let persisted = std::fs::read_to_string(&runtime).unwrap();

        assert_eq!(runtime, runtime_objectives_path(&workspace));
        assert!(persisted.contains("\"objectives\": []"));
    }
}
