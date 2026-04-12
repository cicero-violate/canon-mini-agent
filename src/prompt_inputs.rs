use anyhow::{bail, Context, Result};
use canon_llm::{config::LlmEndpoint, tab_management::TabManagerHandle, ws_server::WsBridge};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::constants::{INVARIANTS_FILE, MASTER_PLAN_FILE, OBJECTIVES_FILE, SPEC_FILE};

use crate::prompts::{
    single_role_diagnostics_prompt, single_role_executor_prompt, single_role_planner_prompt,
    single_role_verifier_prompt, AgentPromptKind,
};

#[derive(Clone)]
pub struct LaneConfig {
    pub index: usize,
    pub endpoint: LlmEndpoint,
    pub plan_file: String,
    pub label: String,
    pub tabs: TabManagerHandle,
}

pub struct OrchestratorContext<'a> {
    pub lanes: &'a [LaneConfig],
    pub workspace: &'a PathBuf,
    pub bridge: &'a WsBridge,
    pub tabs_planner: &'a TabManagerHandle,
    pub tabs_solo: &'a TabManagerHandle,
    pub tabs_diagnostics: &'a TabManagerHandle,
    pub tabs_verify: &'a TabManagerHandle,
    pub planner_ep: &'a LlmEndpoint,
    pub solo_ep: &'a LlmEndpoint,
    pub diagnostics_ep: &'a LlmEndpoint,
    pub verifier_ep: &'a LlmEndpoint,
    pub master_plan_path: &'a Path,
    pub violations_path: &'a Path,
    pub diagnostics_path: &'a Path,
}

pub struct PlannerInputs {
    pub summary_text: String,
    pub executor_diff_text: String,
    pub cargo_test_failures: String,
    pub lessons_text: String,
    pub objectives_text: String,
    pub invariants_text: String,
    pub violations_text: String,
    pub diagnostics_text: String,
    pub plan_text: String,
    pub plan_diff_text: String,
}

pub struct ExecutorDiffInputs {
    pub diff_text: String,
}

pub struct SingleRoleInputs {
    pub role: String,
    pub prompt_kind: AgentPromptKind,
    pub primary_input: String,
}

pub struct SingleRoleContext<'a> {
    pub workspace: &'a Path,
    pub spec_path: &'a Path,
    pub master_plan_path: &'a Path,
    pub violations_path: &'a Path,
    pub diagnostics_path: &'a Path,
}

const LESSONS_FILE: &str = "agent_state/lessons.json";

/// Lifecycle of an individual lesson entry.
///
/// `Pending`  — the lesson lives only in `lessons.json` and is injected into the
///              planner/solo prompt at runtime.  The agent is still acting on it
///              from text rather than from code.
///
/// `Encoded`  — the lesson has been hardcoded into the system source (a validation
///              rule, a schema-fix hint, a prompt constant, etc.).  It is excluded
///              from the rendered prompt because the system already embodies it.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LessonEntryStatus {
    #[default]
    Pending,
    Encoded,
}

impl<'de> serde::Deserialize<'de> for LessonEntryStatus {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "encoded" => Ok(LessonEntryStatus::Encoded),
            _ => Ok(LessonEntryStatus::Pending),
        }
    }
}

/// A single lesson item — text plus its encoding lifecycle status.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LessonEntry {
    pub text: String,
    #[serde(default)]
    pub status: LessonEntryStatus,
}

impl LessonEntry {
    pub fn pending(text: impl Into<String>) -> Self {
        LessonEntry { text: text.into(), status: LessonEntryStatus::Pending }
    }
    pub fn is_pending(&self) -> bool {
        self.status == LessonEntryStatus::Pending
    }
}

/// Deserialize `LessonEntry` from either a plain string (old format) or a
/// `{"text": "...", "status": "..."}` object (new format).
impl<'de> serde::Deserialize<'de> for LessonEntry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct EntryVisitor;
        impl<'de> serde::de::Visitor<'de> for EntryVisitor {
            type Value = LessonEntry;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a string or {{\"text\":\"...\",\"status\":\"...\"}} object")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<LessonEntry, E> {
                Ok(LessonEntry::pending(v))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<LessonEntry, E> {
                Ok(LessonEntry::pending(v))
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<LessonEntry, M::Error> {
                let mut text: Option<String> = None;
                let mut status = LessonEntryStatus::Pending;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "text" => text = Some(map.next_value()?),
                        "status" => status = map.next_value()?,
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(LessonEntry {
                    text: text.unwrap_or_default(),
                    status,
                })
            }
        }
        d.deserialize_any(EntryVisitor)
    }
}

const ENCODING_INSTRUCTIONS: &str = "\
To encode a lesson permanently into the system source (so it no longer needs\n\
to live in this prompt):\n\
  failure_pattern / fix entries  →  add to `schema_fix_hint()` or\n\
      `sequence_workflow_note()` in src/lessons.rs, or add a validation rule\n\
      to `first_missing_field_for_action()` in src/tool_schema.rs.\n\
  success_sequence / required_action entries  →  add to the relevant agent\n\
      prompt constant in src/prompts.rs, or add a runtime enforcement check\n\
      in src/app.rs (see `enforce_diagnostics_python` as a model).\n\
After encoding, call `lessons encode` with the entry text to mark its status\n\
as `encoded`.  Encoded entries are excluded from the rendered prompt because\n\
the system already embodies them structurally.";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct LessonsArtifact {
    #[serde(default)]
    pub summary: String,
    /// Recurring failure patterns observed in action logs.
    #[serde(default)]
    pub failures: Vec<LessonEntry>,
    /// Concrete fixes / schema corrections for each failure pattern.
    #[serde(default)]
    pub fixes: Vec<LessonEntry>,
    /// Forward-looking workflow instructions derived from success sequences.
    #[serde(default)]
    pub required_actions: Vec<LessonEntry>,
    /// How to graduate a lesson from runtime-prompt injection to system source.
    /// Set automatically; do not edit manually.
    #[serde(default = "default_encoding_instructions")]
    pub encoding_instructions: String,
}

fn default_encoding_instructions() -> String {
    ENCODING_INSTRUCTIONS.to_string()
}

pub fn read_text_or_empty(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

pub fn read_required_text(path: impl AsRef<Path>, name: &str) -> Result<String> {
    std::fs::read_to_string(path.as_ref()).with_context(|| format!("failed to read {name}"))
}

fn render_lessons_list(title: &str, items: &[LessonEntry]) -> Option<String> {
    // Only show pending entries — encoded ones are already in the system source.
    let pending: Vec<&str> = items
        .iter()
        .filter(|e| e.is_pending())
        .map(|e| e.text.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if pending.is_empty() {
        return None;
    }
    Some(format!(
        "{title}:\n{}",
        pending
            .iter()
            .map(|t| format!("- {t}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

fn render_lessons_artifact(artifact: &LessonsArtifact) -> String {
    let mut sections = Vec::new();
    let summary = artifact.summary.trim();
    if !summary.is_empty() {
        sections.push(format!("Summary:\n{summary}"));
    }
    if let Some(section) = render_lessons_list("Failures", &artifact.failures) {
        sections.push(section);
    }
    if let Some(section) = render_lessons_list("Fixes", &artifact.fixes) {
        sections.push(section);
    }
    if let Some(section) = render_lessons_list("Required actions", &artifact.required_actions) {
        sections.push(section);
    }
    sections.join("\n\n")
}

const RENAME_CANDIDATES_FILE: &str = "state/rename_candidates.json";

pub fn read_rename_candidates_or_empty(workspace: &Path) -> String {
    let raw = read_text_or_empty(workspace.join(RENAME_CANDIDATES_FILE));
    if raw.trim().is_empty() {
        return String::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return raw;
    };
    let Some(candidates) = v.get("candidates").and_then(|c| c.as_array()) else {
        return raw;
    };
    if candidates.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    for c in candidates {
        let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let file = c.get("file").and_then(|v| v.as_str()).unwrap_or("?");
        let line = c.get("span").and_then(|s| s.get("line")).and_then(|v| v.as_u64());
        let score = c.get("score").and_then(|v| v.as_u64()).unwrap_or(0);
        let reasons = c.get("reasons")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        let loc = match line {
            Some(l) => format!("{file}:{l}"),
            None => file.to_string(),
        };
        lines.push(format!("- [{score}] `{name}` ({kind}) at {loc} — {reasons}"));
    }
    lines.join("\n")
}

/// Read state/reports/complexity/latest.json and return the top `limit` hotspots
/// as compact text for prompt injection. Returns empty string if report is absent.
pub fn read_complexity_hotspots(workspace: &Path, limit: usize) -> String {
    let path = complexity_report_path(workspace);
    let Some(report) = load_complexity_report(&path) else {
        return String::new();
    };
    let Some(top) = report.get("global_top").and_then(|v| v.as_array()) else {
        return String::new();
    };
    if top.is_empty() {
        return String::new();
    }
    // Show the objective formula once so the LLM knows what the score means.
    let mut out = String::from(
        "Complexity objective: min(B) + min(R)  →  objective_score = 0.6*B_norm + 0.4*R_norm ∈ [0,1]\n\
         B_norm=mir_blocks/max  R_norm=stmt_density/max  (higher score = higher-value refactor target)\n\
         Loop: Detect(this report) → Propose(LLM) → Apply(patch/rename) → Verify(build+test)\n",
    );
    for item in top.iter().take(limit.max(1)) {
        out.push_str(&format_complexity_hotspot_line(item));
    }
    out
}

fn complexity_report_path(workspace: &Path) -> std::path::PathBuf {
    workspace
        .join("state")
        .join("reports")
        .join("complexity")
        .join("latest.json")
}

fn load_complexity_report(path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw).ok()
}

fn format_complexity_hotspot_line(item: &serde_json::Value) -> String {
    let symbol = item.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
    let file = item.get("file").and_then(|v| v.as_str()).unwrap_or("?");
    let line = item.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
    let obj_score = item
        .get("objective_score")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let blocks = item
        .get("mir_blocks")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let density = item
        .get("stmt_density")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    format!("  [obj:{obj_score} B:{blocks} R_density:{density}] {symbol} ({file}:{line})\n")
}

pub fn read_lessons_or_empty(workspace: &Path) -> String {
    let raw = read_text_or_empty(workspace.join(LESSONS_FILE));
    if raw.trim().is_empty() {
        return raw;
    }
    match serde_json::from_str::<LessonsArtifact>(&raw) {
        Ok(artifact) => render_lessons_artifact(&artifact),
        Err(_) => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "canon-mini-agent-{label}-{}-{}",
            std::process::id(),
            unique
        ))
    }

    #[test]
    fn read_lessons_or_empty_renders_structured_json_for_prompts() {
        let workspace = temp_workspace("lessons-structured");
        fs::create_dir_all(workspace.join("agent_state")).unwrap();
        fs::write(
            workspace.join(LESSONS_FILE),
            r#"{
  "summary": "Recent solo cycles found missing objective/plan follow-up when lessons exist.",
  "failures": ["Lessons present without follow-up state update"],
  "fixes": ["Added explicit cycle-end enforcement signal"],
  "required_actions": ["Add focused prompt-load coverage for structured lessons"]
}"#,
        )
        .unwrap();

        let rendered = read_lessons_or_empty(&workspace);

        assert!(rendered.contains("Summary:"));
        assert!(rendered.contains("Failures:"));
        assert!(rendered.contains("Fixes:"));
        assert!(rendered.contains("Required actions:"));
        assert!(rendered.contains("- Lessons present without follow-up state update"));
    }

    #[test]
    fn read_lessons_or_empty_preserves_plaintext_lessons() {
        let workspace = temp_workspace("lessons-plaintext");
        fs::create_dir_all(workspace.join("agent_state")).unwrap();
        fs::write(
            workspace.join(LESSONS_FILE),
            "plain text lesson entry for prompt injection",
        )
        .unwrap();

        let rendered = read_lessons_or_empty(&workspace);

        assert_eq!(rendered, "plain text lesson entry for prompt injection");
    }
}

fn is_done_like_status(status: &str) -> bool {
    crate::issues::is_done_like_status(status)
}

fn is_ready_status(status: &str) -> bool {
    status.trim().to_ascii_lowercase() == "ready"
}

/// Extract the top-N ready tasks from PLAN.json and format them for the executor prompt.
///
/// Returns a formatted string listing each ready task as:
///   [priority] id: title
///     → step 1
///     → step 2 (first two steps only)
///
/// Returns "(no ready tasks)" when PLAN.json is missing, empty, or has no ready tasks.
pub fn read_ready_tasks(workspace: &Path, limit: usize) -> String {
    let plan_path = workspace.join(crate::constants::MASTER_PLAN_FILE);
    let raw = match std::fs::read_to_string(&plan_path) {
        Ok(s) => s,
        Err(_) => return "(no ready tasks)".to_string(),
    };
    if raw.trim().is_empty() {
        return "(no ready tasks)".to_string();
    }
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return "(no ready tasks)".to_string();
    };
    let Some(tasks) = value.get("tasks").and_then(Value::as_array) else {
        return "(no ready tasks)".to_string();
    };

    let ready: Vec<&Value> = tasks
        .iter()
        .filter(|t| {
            t.get("status")
                .and_then(Value::as_str)
                .map(is_ready_status)
                .unwrap_or(false)
        })
        .take(limit)
        .collect();

    if ready.is_empty() {
        return "(no ready tasks)".to_string();
    }

    let mut out = String::new();
    for task in &ready {
        let id = task.get("id").and_then(Value::as_str).unwrap_or("?");
        let priority = task.get("priority").and_then(Value::as_str).unwrap_or("?");
        let title = task.get("title").and_then(Value::as_str).unwrap_or("(no title)");
        out.push_str(&format!("[{priority}] {id}: {title}\n"));
        if let Some(steps) = task.get("steps").and_then(Value::as_array) {
            for step in steps.iter().take(2) {
                if let Some(s) = step.as_str() {
                    out.push_str(&format!("  → {s}\n"));
                }
            }
        }
    }
    out.trim_end().to_string()
}

pub fn filter_pending_plan_json(raw: &str) -> String {
    if raw.trim().is_empty() {
        return "(no pending plan tasks)".to_string();
    }
    let Ok(mut value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let Some(obj) = value.as_object_mut() else {
        return raw.to_string();
    };
    let Some(tasks) = obj.get("tasks").and_then(Value::as_array) else {
        return raw.to_string();
    };

    let pending_tasks: Vec<Value> = tasks
        .iter()
        .filter(|task| {
            !task
                .get("status")
                .and_then(Value::as_str)
                .map(is_done_like_status)
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    if pending_tasks.is_empty() {
        return "(no pending plan tasks)".to_string();
    }

    let pending_ids: std::collections::HashSet<String> = pending_tasks
        .iter()
        .filter_map(|task| task.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();

    obj.insert("tasks".to_string(), Value::Array(pending_tasks));
    if let Some(edges) = obj
        .get("dag")
        .and_then(Value::as_object)
        .and_then(|dag| dag.get("edges"))
        .and_then(Value::as_array)
    {
        let filtered_edges: Vec<Value> = edges
            .iter()
            .filter(|edge| {
                let from = edge.get("from").and_then(Value::as_str);
                let to = edge.get("to").and_then(Value::as_str);
                match (from, to) {
                    (Some(from), Some(to)) => pending_ids.contains(from) && pending_ids.contains(to),
                    _ => false,
                }
            })
            .cloned()
            .collect();
        obj.insert("dag".to_string(), serde_json::json!({ "edges": filtered_edges }));
    }

    serde_json::to_string_pretty(&value).unwrap_or_else(|_| raw.to_string())
}

pub fn filter_invariants_json(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let Some(invariants) = value.get("invariants").and_then(Value::as_array) else {
        return raw.to_string();
    };
    if invariants.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for inv in invariants {
        let id = inv.get("id").and_then(Value::as_str).unwrap_or("?");
        let title = inv.get("title").and_then(Value::as_str).unwrap_or("(no title)");
        let level = inv.get("level").and_then(Value::as_str).unwrap_or("?");
        let category = inv.get("category").and_then(Value::as_str).unwrap_or("");
        let scope = if category.is_empty() { String::new() } else { format!("  ({})", category) };
        out.push_str(&format!("[{level}]  {id}  —  {title}{scope}\n"));
    }
    out.push_str("Full detail: {\"action\":\"read_file\",\"path\":\"INVARIANTS.json\"}");
    out
}

pub fn filter_active_violations_json(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let Some(violations) = value.get("violations").and_then(Value::as_array) else {
        return raw.to_string();
    };
    if violations.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for v in violations {
        let id = v.get("id").and_then(Value::as_str).unwrap_or("?");
        let title = v.get("title").and_then(Value::as_str)
            .or_else(|| v.get("description").and_then(Value::as_str))
            .unwrap_or("(no title)");
        let severity = v.get("severity").and_then(Value::as_str).unwrap_or("error");
        out.push_str(&format!("[{severity}]  {id}  —  {title}\n"));
    }
    out.push_str("Full detail: {\"action\":\"read_file\",\"path\":\"VIOLATIONS.json\"}");
    out
}

pub fn filter_active_diagnostics_json(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let Some(failures) = value.get("ranked_failures").and_then(Value::as_array) else {
        return raw.to_string();
    };
    if failures.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (rank, f) in failures.iter().enumerate() {
        let id = f.get("id").and_then(Value::as_str).unwrap_or("?");
        let title = f.get("title").and_then(Value::as_str)
            .or_else(|| f.get("description").and_then(Value::as_str))
            .unwrap_or("(no title)");
        let severity = f.get("severity").and_then(Value::as_str).unwrap_or("?");
        out.push_str(&format!("[{}] [{severity}]  {id}  —  {title}\n", rank + 1));
    }
    out.push_str("Full detail: {\"action\":\"read_file\",\"path\":\"DIAGNOSTICS.json\"}");
    out
}

/// Returns a human-readable explanation of which failure is missing source
/// validation and what keywords are accepted, for use in tool result messages.
pub(crate) fn describe_missing_source_validation(failures: &[Value]) -> String {
    let keywords = &[
        "read_file",
        "verified against current source",
        "validated against current source",
        "source validation",
    ];
    for failure in failures {
        let id = failure.get("id").and_then(Value::as_str).unwrap_or("?");
        let entries: Vec<&str> = failure
            .get("evidence")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let has_validation = entries.iter().any(|e| {
            let n = e.to_ascii_lowercase();
            keywords.iter().any(|kw| n.contains(kw))
                && !n.contains("without source validation")
                && !n.contains("no source validation")
        });
        if !has_validation {
            let shown: Vec<String> = entries.iter().map(|e| format!("    - {e}")).collect();
            let shown_text = if shown.is_empty() {
                "    (no evidence entries)".to_string()
            } else {
                shown.join("\n")
            };
            return format!(
                "ranked_failures[\"{id}\"] has no current-source validation marker.\n\
                 At least one evidence entry must contain one of: {kw_list}.\n\
                 Current evidence:\n{shown_text}\n\
                 Add an entry such as: \"read_file <path>:<lines> — <what you observed>\"",
                kw_list = keywords
                    .iter()
                    .map(|k| format!("\"{k}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }
    "ranked_failures missing current-source validation (no failures to inspect)".to_string()
}

fn diagnostics_have_current_source_validation(failures: &[Value]) -> bool {
    failures.iter().all(|failure| {
        failure
            .get("evidence")
            .and_then(Value::as_array)
            .map(|entries| {
                entries.iter().filter_map(Value::as_str).any(|entry| {
                    let normalized = entry.to_ascii_lowercase();
                    normalized.contains("read_file")
                        || normalized.contains("verified against current source")
                        || normalized.contains("validated against current source")
                        || (
                            normalized.contains("source validation")
                                && !normalized.contains("without source validation")
                                && !normalized.contains("no source validation")
                        )
                })
            })
            .unwrap_or(false)
    })
}

fn violations_are_verified_and_empty(raw_violations_text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(raw_violations_text) else {
        return false;
    };
    value.get("status").and_then(Value::as_str) == Some("verified")
        && value
            .get("violations")
            .and_then(Value::as_array)
            .map(|violations| violations.is_empty())
            .unwrap_or(false)
}

pub(crate) fn reconcile_diagnostics_report(
    raw_diagnostics_text: &str,
    raw_violations_text: &str,
) -> String {
    if !violations_are_verified_and_empty(raw_violations_text) {
        return raw_diagnostics_text.to_string();
    }

    // Use typed round-trip so no unrecognised fields can be introduced.
    let Ok(mut report) = serde_json::from_str::<crate::reports::DiagnosticsReport>(raw_diagnostics_text) else {
        return raw_diagnostics_text.to_string();
    };

    report.status = "verified".to_string();
    report.ranked_failures = Vec::new();
    report.planner_handoff = vec![
        "No active diagnostics contradictions remain after verifier reconciliation; \
         treat prior ranked failures as resolved unless new current-source evidence is recorded."
            .to_string(),
    ];

    serde_json::to_string_pretty(&report).unwrap_or_else(|_| raw_diagnostics_text.to_string())
}

pub(crate) fn sanitize_diagnostics_for_planner(
    raw_diagnostics_text: &str,
    raw_violations_text: &str,
) -> String {
    if raw_diagnostics_text.trim().is_empty() {
        return "(no diagnostics)".to_string();
    }

    let Ok(value) = serde_json::from_str::<Value>(raw_diagnostics_text) else {
        return "(invalid diagnostics: not valid json)".to_string();
    };

    let Some(ranked_failures) = value.get("ranked_failures").and_then(Value::as_array) else {
        return "(invalid diagnostics: missing ranked_failures)".to_string();
    };

    if ranked_failures.is_empty() {
        return "(no active diagnostics failures)".to_string();
    }

    if violations_are_verified_and_empty(raw_violations_text) {
        return "(suppressed stale diagnostics: verifier state is authoritative and VIOLATIONS.json is verified with no active violations)".to_string();
    }

    if diagnostics_have_current_source_validation(ranked_failures) {
        return format!(
            "(SOURCE-VALIDATED DIAGNOSTICS — current-source evidence is present; still verify before creating tasks)\n{}",
            raw_diagnostics_text
        );
    }

    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("Diagnostics failures suppressed until current-source validation is recorded.");
    format!(
        "(suppressed stale or unverified diagnostics: ranked_failures present without current-source validation evidence)\n{}",
        summary
    )
}

pub fn lane_summary_text(lanes: &[LaneConfig], verifier_summary: &[String]) -> String {
    lanes
        .iter()
        .map(|lane| format!("{}={}", lane.label, verifier_summary[lane.index]))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn load_executor_diff_inputs(
    workspace: &Path,
    last_executor_diff: &mut String,
    max_lines: usize,
) -> ExecutorDiffInputs {
    let current_executor_diff = executor_diff(workspace, max_lines);
    let diff_text = diff_since_last_cycle(&current_executor_diff, last_executor_diff);
    *last_executor_diff = current_executor_diff;
    ExecutorDiffInputs { diff_text }
}

pub struct VerifierPromptInputs {
    pub executor_diff_text: String,
    pub cargo_test_failures: String,
}

pub fn load_verifier_prompt_inputs(
    lanes: &[LaneConfig],
    workspace: &Path,
    verifier_summary: &[String],
    last_executor_diff: &mut String,
    cargo_test_failures: String,
) -> VerifierPromptInputs {
    let _summary_text = lane_summary_text(lanes, verifier_summary);
    let executor_diff_text = load_executor_diff_inputs(workspace, last_executor_diff, 400).diff_text;
    VerifierPromptInputs {
        executor_diff_text,
        cargo_test_failures,
    }
}

pub fn load_planner_inputs(
    lanes: &[LaneConfig],
    workspace: &Path,
    verifier_summary: &[String],
    last_plan_text: &str,
    last_executor_diff: &mut String,
    cargo_test_failures: String,
    violations_path: &Path,
    diagnostics_path: &Path,
    master_plan_path: &Path,
) -> PlannerInputs {
    let summary_text = lane_summary_text(lanes, verifier_summary);
    let executor_diff_text = load_executor_diff_inputs(workspace, last_executor_diff, 400).diff_text;
    let lessons_text = read_lessons_or_empty(workspace);
    let objectives_text = crate::objectives::read_objectives_compact(&workspace.join(OBJECTIVES_FILE));
    let invariants_text = filter_invariants_json(&read_text_or_empty(workspace.join(INVARIANTS_FILE)));
    let raw_violations_text = read_text_or_empty(violations_path);
    let violations_text = filter_active_violations_json(&raw_violations_text);
    let raw_diagnostics_text = read_text_or_empty(diagnostics_path);
    let diagnostics_text = sanitize_diagnostics_for_planner(&raw_diagnostics_text, &raw_violations_text);
    let plan_text = read_text_or_empty(master_plan_path);
    let plan_diff_text = plan_diff(last_plan_text, &plan_text, 400);
    PlannerInputs {
        summary_text,
        executor_diff_text,
        cargo_test_failures,
        lessons_text,
        objectives_text,
        invariants_text,
        violations_text,
        diagnostics_text,
        plan_text,
        plan_diff_text,
    }
}

pub enum SingleRoleRead {
    Objectives,
    Invariants,
    Lessons,
    Violations,
    Diagnostics,
    Issues,
    MasterPlan,
    Spec,
}

impl SingleRoleContext<'_> {
    pub fn read(&self, kind: SingleRoleRead) -> Result<String> {
        let text = match kind {
            SingleRoleRead::Objectives => {
                crate::objectives::read_objectives_compact(&self.workspace.join(OBJECTIVES_FILE))
            }
            SingleRoleRead::Invariants => {
                filter_invariants_json(&read_text_or_empty(self.workspace.join(INVARIANTS_FILE)))
            }
            SingleRoleRead::Lessons => read_lessons_or_empty(self.workspace),
            SingleRoleRead::Violations => {
                filter_active_violations_json(&read_text_or_empty(self.violations_path))
            }
            SingleRoleRead::Diagnostics => {
                filter_active_diagnostics_json(&read_text_or_empty(self.diagnostics_path))
            }
            SingleRoleRead::Issues => crate::issues::read_top_open_issues(self.workspace, 10),
            SingleRoleRead::MasterPlan => {
                filter_pending_plan_json(&read_text_or_empty(self.master_plan_path))
            }
            SingleRoleRead::Spec => read_required_text(self.spec_path, SPEC_FILE)?,
        };
        Ok(text)
    }

    pub fn read_executor_diff(&self, max_lines: usize) -> String {
        executor_diff(self.workspace, max_lines)
    }

    // removed lane_plan_list method (lane plans deleted)
}

pub fn load_single_role_inputs(
    ctx: &SingleRoleContext<'_>,
    is_verifier: bool,
    is_diagnostics: bool,
    is_planner: bool,
) -> Result<SingleRoleInputs> {
    let (role, prompt_kind) = if is_verifier {
        ("verifier", AgentPromptKind::Verifier)
    } else if is_diagnostics {
        ("diagnostics", AgentPromptKind::Diagnostics)
    } else if is_planner {
        ("mini_planner", AgentPromptKind::Planner)
    } else {
        ("executor", AgentPromptKind::Executor)
    };

    let primary_input_path = if is_verifier || is_planner {
        ctx.spec_path
    } else {
        ctx.master_plan_path
    };
    let primary_input_name = if is_verifier || is_planner {
        SPEC_FILE.to_string()
    } else {
        MASTER_PLAN_FILE.to_string()
    };
    let primary_input = read_required_text(primary_input_path, &primary_input_name)?;
    if primary_input.trim().is_empty() {
        bail!("input file is empty — write content into {primary_input_name} before running");
    }

    Ok(SingleRoleInputs {
        role: role.to_string(),
        prompt_kind,
        primary_input,
    })
}

pub fn build_single_role_prompt(
    ctx: &SingleRoleContext<'_>,
    inputs: &SingleRoleInputs,
    cargo_test_failures: &str,
) -> Result<String> {
    let prompt = match inputs.prompt_kind {
        AgentPromptKind::Verifier => {
            build_verifier_role_prompt(ctx, inputs, cargo_test_failures)?
        }
        AgentPromptKind::Diagnostics => {
            build_diagnostics_role_prompt(ctx, cargo_test_failures)?
        }
        AgentPromptKind::Planner => {
            build_planner_role_prompt(ctx, inputs, cargo_test_failures)?
        }
        AgentPromptKind::Executor => build_executor_role_prompt(ctx)?,
        AgentPromptKind::Solo => {
            bail!("solo role is only supported in orchestration mode")
        }
    };
    Ok(prompt)
}

fn build_verifier_role_prompt(
    ctx: &SingleRoleContext<'_>,
    inputs: &SingleRoleInputs,
    cargo_test_failures: &str,
) -> Result<String> {
    let invariants = ctx.read(SingleRoleRead::Invariants)?;
    let objectives = ctx.read(SingleRoleRead::Objectives)?;
    let executor_diff_text = ctx.read_executor_diff(400);
    Ok(single_role_verifier_prompt(
        &inputs.primary_input,
        &objectives,
        &invariants,
        &executor_diff_text,
        cargo_test_failures,
    ))
}

fn build_diagnostics_role_prompt(
    ctx: &SingleRoleContext<'_>,
    cargo_test_failures: &str,
) -> Result<String> {
    let violations = ctx.read(SingleRoleRead::Violations)?;
    let objectives = ctx.read(SingleRoleRead::Objectives)?;
    Ok(single_role_diagnostics_prompt(
        &violations,
        &objectives,
        cargo_test_failures,
    ))
}

fn build_planner_role_prompt(
    ctx: &SingleRoleContext<'_>,
    inputs: &SingleRoleInputs,
    cargo_test_failures: &str,
) -> Result<String> {
    let violations = ctx.read(SingleRoleRead::Violations)?;
    let raw_diagnostics = ctx.read(SingleRoleRead::Diagnostics)?;
    let diagnostics = sanitize_diagnostics_for_planner(&raw_diagnostics, &violations);
    let lessons = ctx.read(SingleRoleRead::Lessons)?;
    let objectives = ctx.read(SingleRoleRead::Objectives)?;
    let issues = ctx.read(SingleRoleRead::Issues)?;
    let invariants = ctx.read(SingleRoleRead::Invariants)?;
    Ok(single_role_planner_prompt(
        &inputs.primary_input,
        &objectives,
        &lessons,
        &invariants,
        &violations,
        &diagnostics,
        &issues,
        cargo_test_failures,
    ))
}

fn build_executor_role_prompt(ctx: &SingleRoleContext<'_>) -> Result<String> {
    let spec = ctx.read(SingleRoleRead::Spec)?;
    let master_plan = ctx.read(SingleRoleRead::MasterPlan)?;
    let violations = ctx.read(SingleRoleRead::Violations)?;
    let diagnostics = ctx.read(SingleRoleRead::Diagnostics)?;
    let invariants = ctx.read(SingleRoleRead::Invariants)?;
    Ok(single_role_executor_prompt(
        &spec,
        &master_plan,
        &violations,
        &diagnostics,
        &invariants,
    ))
}

fn executor_diff_unavailable(reason: &str) -> String {
    format!("(executor diff unavailable: {reason})")
}

/// Public wrapper so solo phase can compute plan diffs without duplicating logic.
pub fn solo_plan_diff(old_text: &str, new_text: &str, max_lines: usize) -> String {
    plan_diff(old_text, new_text, max_lines)
}

fn plan_diff(old_text: &str, new_text: &str, max_lines: usize) -> String {
    if old_text.is_empty() {
        let mut out = String::from("+++ PLAN.json (initial)\n");
        for (idx, line) in new_text.lines().enumerate() {
            if idx >= max_lines {
                out.push_str("... (truncated)\n");
                break;
            }
            out.push_str("+ ");
            out.push_str(line);
            out.push('\n');
        }
        return out;
    }
    if old_text == new_text {
        return "(no changes)".to_string();
    }
    let mut out = String::new();
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let mut i = 0usize;
    let mut j = 0usize;
    let mut emitted = 0usize;
    while i < old_lines.len() || j < new_lines.len() {
        if emitted >= max_lines {
            out.push_str("... (truncated)\n");
            break;
        }
        match (old_lines.get(i), new_lines.get(j)) {
            (Some(ol), Some(nl)) if ol == nl => {
                i += 1;
                j += 1;
            }
            (Some(ol), Some(nl)) => {
                out.push_str("- ");
                out.push_str(ol);
                out.push('\n');
                out.push_str("+ ");
                out.push_str(nl);
                out.push('\n');
                i += 1;
                j += 1;
                emitted += 2;
            }
            (Some(ol), None) => {
                out.push_str("- ");
                out.push_str(ol);
                out.push('\n');
                i += 1;
                emitted += 1;
            }
            (None, Some(nl)) => {
                out.push_str("+ ");
                out.push_str(nl);
                out.push('\n');
                j += 1;
                emitted += 1;
            }
            (None, None) => break,
        }
    }
    out
}

fn diff_since_last_cycle(current: &str, last: &str) -> String {
    if current.trim().is_empty() {
        return "(no changes)".to_string();
    }
    if current == last {
        return "(no changes)".to_string();
    }
    if last.trim().is_empty() {
        return current.to_string();
    }
    if current.starts_with("(") {
        return current.to_string();
    }
    let last_lines: std::collections::HashSet<&str> = last.lines().collect();
    let mut out_lines = Vec::new();
    for line in current.lines() {
        if !last_lines.contains(line) {
            out_lines.push(line);
        }
    }
    if out_lines.is_empty() {
        "(no changes)".to_string()
    } else {
        let mut out = out_lines.join("\n");
        out.push('\n');
        out
    }
}

fn executor_diff(workspace: &Path, max_lines: usize) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(workspace).args(["diff", "--name-only"]);
    let Ok(output) = cmd.output() else {
        return executor_diff_unavailable("failed to run git diff --name-only");
    };
    if !output.status.success() {
        return executor_diff_unavailable("git diff --name-only failed");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let files = executor_diff_files(&text);
    if files.is_empty() {
        return "(no executor diff)".to_string();
    }
    let mut diff_cmd = std::process::Command::new("git");
    diff_cmd
        .current_dir(workspace)
        .arg("diff")
        .arg("--unified=3")
        .arg("--")
        .args(&files);
    let Ok(diff_out) = diff_cmd.output() else {
        return executor_diff_unavailable("failed to run git diff");
    };
    if !diff_out.status.success() {
        return executor_diff_unavailable("git diff failed");
    }
    let diff_text = String::from_utf8_lossy(&diff_out.stdout);
    if diff_text.trim().is_empty() {
        return "(no executor diff)".to_string();
    }
    render_executor_diff(&diff_text, max_lines)
}

fn executor_diff_files<'a>(text: &'a str) -> Vec<&'a str> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !is_executor_diff_excluded(line))
        .collect()
}

fn is_executor_diff_excluded(line: &str) -> bool {
    line.starts_with("PLAN.json")
        || line.starts_with("PLAN.md")
        || line.starts_with("PLANS/")
        || line == "VIOLATIONS.json"
        || line == "DIAGNOSTICS.json"
}

fn render_executor_diff(diff_text: &str, max_lines: usize) -> String {
    let mut out = String::new();
    for (idx, line) in diff_text.lines().enumerate() {
        if idx >= max_lines {
            out.push_str("... (truncated)\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod diagnostics_filter_tests {
    use super::{
        filter_active_diagnostics_json, filter_active_violations_json, filter_invariants_json,
        filter_pending_plan_json, sanitize_diagnostics_for_planner,
    };

    const NON_AUTHORITATIVE_VIOLATIONS: &str = r#"{}"#;

    const VERIFIED_EMPTY_VIOLATIONS: &str = r#"{
  "status": "verified",
  "summary": "no current violations",
  "violations": []
}"#;

    #[test]
    fn sanitize_diagnostics_suppresses_unverified_ranked_failures() {
        let raw = r#"{
  "status": "critical_failure",
  "summary": "diagnostics found a stale issue",
  "ranked_failures": [
    {
      "id": "D1",
      "evidence": ["old report without source validation"]
    }
  ]
}"#;

        let sanitized = sanitize_diagnostics_for_planner(raw, NON_AUTHORITATIVE_VIOLATIONS);
        assert!(sanitized.contains("suppressed stale or unverified diagnostics"));
        assert!(sanitized.contains("diagnostics found a stale issue"));
    }

    #[test]
    fn sanitize_diagnostics_allows_source_validated_failures() {
        let raw = r#"{
  "status": "critical_failure",
  "summary": "validated diagnostics",
  "ranked_failures": [
    {
      "id": "D1",
      "evidence": ["read_file src/app.rs verified against current source"]
    }
  ]
}"#;

        let sanitized = sanitize_diagnostics_for_planner(raw, NON_AUTHORITATIVE_VIOLATIONS);
        assert!(sanitized.contains("SOURCE-VALIDATED DIAGNOSTICS"));
        assert!(sanitized.contains("validated diagnostics"));
    }

    #[test]
    fn sanitize_diagnostics_suppresses_when_violations_are_verified_and_empty() {
        let raw = r#"{
  "status": "needs_repair",
  "summary": "stale contradiction remains in persisted diagnostics",
  "ranked_failures": [
    {
      "id": "D1",
      "evidence": ["read_file VIOLATIONS.json:1-5 verified against current source"]
    }
  ]
}"#;

        let sanitized = sanitize_diagnostics_for_planner(raw, VERIFIED_EMPTY_VIOLATIONS);
        assert!(sanitized.contains("suppressed stale diagnostics"));
        assert!(sanitized.contains("VIOLATIONS.json is verified with no active violations"));
    }

    #[test]
    fn filter_pending_plan_json_removes_done_tasks() {
        let raw = r#"{
  "version": 1,
  "status": "in_progress",
  "tasks": [
    {"id": "T1", "status": "done"},
    {"id": "T2", "status": "todo"}
  ],
  "dag": { "edges": [ {"from":"T1","to":"T2"}, {"from":"T2","to":"T1"} ] }
}"#;
        let filtered = filter_pending_plan_json(raw);
        assert!(filtered.contains("\"id\": \"T2\""));
        assert!(!filtered.contains("\"id\": \"T1\""));
        assert!(!filtered.contains("\"from\": \"T1\""));
    }

    #[test]
    fn filter_pending_plan_json_reports_none_when_all_done() {
        let raw = r#"{
  "tasks": [
    {"id":"T1","status":"done"},
    {"id":"T2","status":"complete"}
  ]
}"#;
        assert_eq!(filter_pending_plan_json(raw), "(no pending plan tasks)");
    }

    #[test]
    fn filter_invariants_json_derives_compact_lines() {
        let raw = r#"{"version":1,"invariants":[
            {"id":"I1","title":"Workspace Isolation","level":"critical","category":"scope"},
            {"id":"I2","title":"Handoff Delivery","level":"critical","category":"control-flow"}
        ]}"#;
        let out = filter_invariants_json(raw);
        assert!(out.contains("[critical]  I1  —  Workspace Isolation  (scope)"));
        assert!(out.contains("[critical]  I2  —  Handoff Delivery  (control-flow)"));
        assert!(out.contains("INVARIANTS.json"));
    }

    #[test]
    fn filter_invariants_json_empty_returns_empty() {
        let raw = r#"{"version":1,"invariants":[]}"#;
        assert!(filter_invariants_json(raw).is_empty());
    }

    #[test]
    fn filter_active_violations_json_reports_none_when_empty() {
        let raw = r#"{"status":"verified","violations":[]}"#;
        assert!(filter_active_violations_json(raw).is_empty());
    }

    #[test]
    fn filter_active_diagnostics_json_reports_none_when_empty() {
        let raw = r#"{"status":"verified","ranked_failures":[]}"#;
        assert!(filter_active_diagnostics_json(raw).is_empty());
    }

    #[test]
    fn build_single_role_prompt_planner_includes_rendered_lessons_from_context() {
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!(
            "canon-mini-agent-single-role-planner-lessons-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(workspace.join("PLANS/default")).unwrap();
        fs::create_dir_all(workspace.join("agent_state")).unwrap();

        fs::write(workspace.join("SPEC.md"), "planner spec body").unwrap();
        fs::write(
            workspace.join("PLANS/OBJECTIVES.json"),
            r#"{"version":1,"objectives":[{"id":"obj_15","title":"OBJ-15","status":"active"}]}"#,
        )
        .unwrap();
        fs::write(workspace.join("INVARIANTS.json"), r#"{"version":1,"invariants":[]}"#).unwrap();
        fs::write(workspace.join("VIOLATIONS.json"), r#"{"status":"verified","violations":[]}"#).unwrap();
        fs::write(
            workspace.join("PLANS/default/diagnostics-default.json"),
            r#"{"status":"verified","ranked_failures":[]}"#,
        )
        .unwrap();
        fs::write(
            workspace.join("agent_state/lessons.json"),
            r#"{
  "summary": "Structured planner lesson summary.",
  "failures": ["Missing writeback coverage"],
  "fixes": ["Add planner-side regression"],
  "required_actions": ["Validate shared prompt-load path"]
}"#,
        )
        .unwrap();

        let spec_path = workspace.join("SPEC.md");
        let master_plan_path = workspace.join("PLAN.json");
        let violations_path = workspace.join("VIOLATIONS.json");
        let diagnostics_path = workspace.join("PLANS/default/diagnostics-default.json");
        fs::write(&master_plan_path, r#"{"version":2,"tasks":[]}"#).unwrap();

        let ctx = super::SingleRoleContext {
            workspace: workspace.as_path(),
            spec_path: spec_path.as_path(),
            master_plan_path: master_plan_path.as_path(),
            violations_path: violations_path.as_path(),
            diagnostics_path: diagnostics_path.as_path(),
        };

        let inputs = super::load_single_role_inputs(&ctx, false, false, true).unwrap();
        let prompt = super::build_single_role_prompt(&ctx, &inputs, "").unwrap();

        assert!(prompt.contains("Summary:\nStructured planner lesson summary."));
        assert!(prompt.contains("Failures:\n- Missing writeback coverage"));
        assert!(prompt.contains("Fixes:\n- Add planner-side regression"));
        assert!(prompt.contains("Required actions:\n- Validate shared prompt-load path"));
    }
}
