/// Lessons synthesis: reads the action log, detects repeated failure patterns,
/// and writes a structured LessonsArtifact to agent_state/lessons.json so that
/// future planner cycles start with accumulated pattern knowledge already in context.
///
/// This runs heuristically — no LLM call required — at the end of each successful
/// diagnostics cycle. The output feeds directly into every planner/solo prompt via
/// `read_lessons_or_empty`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use crate::prompt_inputs::LessonsArtifact;

/// Maximum number of action-log lines to scan (tail of the file).
const MAX_LINES_TO_SCAN: usize = 4000;
/// Minimum occurrences of an error pattern before it becomes a lesson.
const MIN_OCCURRENCES: usize = 2;
/// Maximum lessons per category to avoid prompt bloat.
const MAX_PER_CATEGORY: usize = 5;

pub fn maybe_synthesize_lessons(workspace: &Path) {
    if let Err(e) = synthesize_lessons(workspace) {
        eprintln!("[lessons] synthesis error: {e:#}");
    }
}

fn synthesize_lessons(workspace: &Path) -> anyhow::Result<()> {
    let agent_state_dir = crate::constants::agent_state_dir();
    let log_path = Path::new(agent_state_dir).join("default").join("actions.jsonl");

    if !log_path.exists() {
        return Ok(());
    }

    let entries = read_tail_entries(&log_path, MAX_LINES_TO_SCAN);
    if entries.is_empty() {
        return Ok(());
    }

    let artifact = build_lessons_artifact(&entries);

    // Only write if there's something worth saying.
    if artifact.failures.is_empty() && artifact.fixes.is_empty() && artifact.required_actions.is_empty() {
        return Ok(());
    }

    let json_text = serde_json::to_string_pretty(&artifact)?;
    let lessons_path = workspace.join("agent_state").join("lessons.json");
    if let Some(parent) = lessons_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&lessons_path, &json_text)?;
    eprintln!(
        "[lessons] wrote {} failures, {} fixes, {} required_actions",
        artifact.failures.len(),
        artifact.fixes.len(),
        artifact.required_actions.len()
    );
    Ok(())
}

/// Read the last `max_lines` lines from the file and parse each as a JSON Value.
fn read_tail_entries(path: &Path, max_lines: usize) -> Vec<Value> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().flatten().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Extract the failure reason from a tool result text, normalizing away
/// specific IDs so similar errors group together.
fn extract_error_pattern(text: &str) -> String {
    let stripped = text
        .trim_start_matches("Error executing action: ")
        .trim();

    // Normalize away specific IDs and values while keeping the structural message.
    let normalized = normalize_error(stripped);
    // Cap at 120 chars for grouping key.
    if normalized.len() > 120 {
        normalized[..120].to_string()
    } else {
        normalized
    }
}

fn normalize_error(text: &str) -> String {
    // Remove trailing content after a newline (tool examples appended to errors).
    let first_line = text.lines().next().unwrap_or(text).trim();
    // Strip quoted values and IDs: e.g. "task not found: T_foo_bar" -> "task not found: <id>"
    let mut s = first_line.to_string();
    // "task not found: <id>" / "issue not found: <id>" / "objective not found: <id>"
    for prefix in &["task not found: ", "issue not found: ", "objective not found: ", "issue id already exists: ", "task already exists: ", "objective id already exists: "] {
        if let Some(pos) = s.find(prefix) {
            s = format!("{}{}<id>", &s[..pos], prefix);
            break;
        }
    }
    // "invalid type: string \"...\", expected ..." -> "invalid type: string, expected ..."
    if let Some(start) = s.find("invalid type: string \"") {
        if let Some(end) = s[start + 22..].find('"') {
            let after = &s[start + 22 + end + 1..].to_string();
            s = format!("{}invalid type: string{}", &s[..start], after);
        }
    }
    // "requested_raw=\"...\"" -> strip
    if let Some(pos) = s.find("requested_raw=\"") {
        s.truncate(pos);
        let s = s.trim_end_matches(", ").trim_end_matches(" ").to_string();
        return s;
    }
    s
}

#[derive(Default)]
struct FailureGroup {
    count: usize,
    /// Example (most recent) raw text for this pattern.
    example: String,
}

fn build_lessons_artifact(entries: &[Value]) -> LessonsArtifact {
    let (failure_map, total_failures, scanned_tool_results) =
        collect_failure_groups(entries);

    if failure_map.is_empty() {
        return LessonsArtifact::default();
    }

    let mut sorted = sort_failure_groups(failure_map);

    let mut failures: Vec<String> = Vec::new();
    let mut fixes: Vec<String> = Vec::new();
    let mut required_actions: Vec<String> = Vec::new();
    let mut seen_actions_with_schema_lesson: std::collections::HashSet<String> = Default::default();

    populate_failure_sections(
        &sorted,
        &mut failures,
        &mut fixes,
        &mut required_actions,
        &mut seen_actions_with_schema_lesson,
    );

    // Deduplicate fixes and required_actions (keep insertion order, drop exact dupes).
    let fixes = dedup_vec(fixes);
    let required_actions = dedup_vec(required_actions);

    // Build summary.
    let top_actions = compute_top_actions(&sorted);
    let summary = format!(
        "{total_failures} failures in last {scanned_tool_results} tool calls. Top failure sources: {}.",
        top_actions.join(", ")
    );

    LessonsArtifact {
        summary,
        failures,
        fixes,
        required_actions,
    }
}

fn collect_failure_groups(
    entries: &[Value],
) -> (HashMap<(String, String), FailureGroup>, usize, usize) {
    let mut failure_map: HashMap<(String, String), FailureGroup> = HashMap::new();
    let mut total_failures = 0usize;
    let mut scanned_tool_results = 0usize;

    for entry in entries {
        if entry.get("kind").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }
        if entry.get("phase").and_then(|v| v.as_str()) != Some("result") {
            continue;
        }
        scanned_tool_results += 1;
        let ok = entry.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
        if ok {
            continue;
        }
        total_failures += 1;
        let action = entry
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let text = entry.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let pattern = extract_error_pattern(text);
        let group = failure_map
            .entry((action, pattern.clone()))
            .or_default();
        group.count += 1;
        group.example = pattern;
    }

    (failure_map, total_failures, scanned_tool_results)
}

fn sort_failure_groups(
    failure_map: HashMap<(String, String), FailureGroup>,
) -> Vec<((String, String), FailureGroup)> {
    let mut sorted: Vec<_> = failure_map.into_iter().collect();
    sorted.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.0.cmp(&b.0.0)));
    sorted
}

fn populate_failure_sections(
    sorted: &Vec<((String, String), FailureGroup)>,
    failures: &mut Vec<String>,
    fixes: &mut Vec<String>,
    required_actions: &mut Vec<String>,
    seen_actions_with_schema_lesson: &mut std::collections::HashSet<String>,
) {
    for ((action, pattern), group) in sorted {
        if group.count < MIN_OCCURRENCES {
            continue;
        }
        if failures.len() >= MAX_PER_CATEGORY {
            break;
        }
        let count_label = if group.count == 1 {
            "1 occurrence".to_string()
        } else {
            format!("{} occurrences", group.count)
        };
        failures.push(format!("`{action}` action: {pattern} ({count_label})"));

        if let Some(fix) = schema_fix_hint(action, pattern) {
            if fixes.len() < MAX_PER_CATEGORY {
                fixes.push(fix);
            }
        }

        if seen_actions_with_schema_lesson.insert(action.clone()) && is_schema_error(pattern) {
            required_actions.push(format!(
                "Before emitting a `{action}` action, verify the payload schema against tool_examples (check required fields and nesting)"
            ));
        }
    }
}

fn compute_top_actions(
    sorted: &Vec<((String, String), FailureGroup)>,
) -> Vec<String> {
    let mut by_action: HashMap<String, usize> = HashMap::new();
    for ((action, _), group) in sorted {
        *by_action.entry(action.clone()).or_default() += group.count;
    }
    let mut v: Vec<_> = by_action.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter()
        .take(4)
        .map(|(a, c)| format!("{a} ({c})"))
        .collect()
}

fn is_schema_error(pattern: &str) -> bool {
    pattern.contains("missing")
        || pattern.contains("invalid type")
        || pattern.contains("does not accept")
        || pattern.contains("unexpected")
}

fn schema_fix_hint(action: &str, pattern: &str) -> Option<String> {
    match action {
        "plan" => {
            if pattern.contains("update_task missing task") {
                Some("plan update_task: wrap fields in a `task` object — `{\"op\":\"update_task\",\"task\":{\"id\":\"<id>\",\"status\":\"<status>\"}}`".to_string())
            } else if pattern.contains("create_task") && (pattern.contains("missing task") || pattern.contains("does not accept task_id")) {
                Some("plan create_task: wrap fields in a `task` object using `task.id`, not a top-level `task_id` — `{\"op\":\"create_task\",\"task\":{\"id\":\"<id>\",\"description\":\"<desc>\"}}`".to_string())
            } else if pattern.contains("replace_plan missing plan") {
                Some("plan replace_plan: requires a top-level `plan` object containing the full plan".to_string())
            } else if pattern.contains("task not found") {
                Some("plan: verify task ID exists in PLAN.json with `read_file` before referencing it in update_task or delete_task".to_string())
            } else {
                None
            }
        }
        "issue" => {
            if pattern.contains("missing 'issue' field") {
                Some("issue create: wrap all fields in an `issue` object — `{\"op\":\"create\",\"issue\":{\"id\":\"<id>\",\"title\":\"<t>\",\"status\":\"open\",\"kind\":\"<k>\",\"priority\":\"<p>\",\"description\":\"<d>\"}}`".to_string())
            } else if pattern.contains("missing field `id`") || pattern.contains("missing field `status`") {
                Some("issue: required fields are id, title, status, kind, priority, description — all must be strings".to_string())
            } else if pattern.contains("invalid type: boolean") {
                Some("issue: all field values must be strings — use `\"true\"` not `true` and quoted values throughout".to_string())
            } else if pattern.contains("missing 'updates' object") {
                Some("issue update: wrap changes in an `updates` object — `{\"op\":\"update\",\"id\":\"<id>\",\"updates\":{\"status\":\"resolved\"}}`".to_string())
            } else if pattern.contains("invalid type") {
                Some("issue: evidence and other array fields must be JSON arrays `[\"...\"]`, not plain strings".to_string())
            } else {
                None
            }
        }
        "objectives" => {
            if pattern.contains("create_objective missing objective") {
                Some("objectives create_objective: wrap fields in an `objective` object — `{\"op\":\"create_objective\",\"objective\":{\"id\":\"<id>\",\"title\":\"<t>\",\"status\":\"active\"}}`".to_string())
            } else if pattern.contains("update_objective missing updates") {
                Some("objectives update_objective: requires both `objective_id` and an `updates` object — `{\"op\":\"update_objective\",\"objective_id\":\"<id>\",\"updates\":{...}}`".to_string())
            } else if pattern.contains("update_objective missing objective_id") {
                Some("objectives update_objective: the `objective_id` field is required at the top level alongside `updates`".to_string())
            } else if pattern.contains("invalid type") {
                Some("objectives: `verification` and `checklist` fields must be JSON arrays `[\"...\"]`, not plain strings".to_string())
            } else {
                None
            }
        }
        "symbol_window" => {
            if pattern.contains("not found in graph") || pattern.contains("not available") {
                Some("symbol_window: if symbol is not found, first run `semantic_map` to discover exact symbol paths, then retry with the full qualified path".to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn dedup_vec(v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.into_iter().filter(|s| seen.insert(s.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool_result(action: &str, ok: bool, text: &str) -> Value {
        serde_json::json!({
            "kind": "tool",
            "phase": "result",
            "action": action,
            "ok": ok,
            "text": text,
        })
    }

    #[test]
    fn synthesizes_failures_from_repeated_errors() {
        let entries = vec![
            make_tool_result("issue", false, "Error executing action: issue create missing 'issue' field"),
            make_tool_result("issue", false, "Error executing action: issue create missing 'issue' field"),
            make_tool_result("issue", false, "Error executing action: issue create missing 'issue' field"),
            make_tool_result("plan", false, "Error executing action: plan update_task missing task\nPlan tool examples:..."),
            make_tool_result("plan", false, "Error executing action: plan update_task missing task\nPlan tool examples:..."),
            make_tool_result("plan", true, "plan ok"),
        ];

        let artifact = build_lessons_artifact(&entries);

        assert!(!artifact.summary.is_empty(), "summary should be set");
        assert!(artifact.failures.iter().any(|f| f.contains("issue")), "issue failure should appear");
        assert!(artifact.failures.iter().any(|f| f.contains("plan")), "plan failure should appear");
        assert!(artifact.fixes.iter().any(|f| f.contains("issue") && f.contains("object")), "issue fix should appear");
        assert!(artifact.fixes.iter().any(|f| f.contains("update_task")), "plan fix should appear");
    }

    #[test]
    fn skips_rare_errors_below_threshold() {
        let entries = vec![
            // Only 1 occurrence — below MIN_OCCURRENCES
            make_tool_result("plan", false, "Error executing action: plan task not found: T_foo"),
        ];

        let artifact = build_lessons_artifact(&entries);
        assert!(artifact.failures.is_empty(), "single-occurrence errors should be skipped");
    }

    #[test]
    fn normalizes_task_not_found_ids() {
        let entries = vec![
            make_tool_result("plan", false, "Error executing action: plan task not found: T_foo_bar"),
            make_tool_result("plan", false, "Error executing action: plan task not found: T_baz_qux"),
        ];

        let artifact = build_lessons_artifact(&entries);
        // Both should collapse to the same pattern and show up as one failure entry
        assert_eq!(artifact.failures.len(), 1, "different IDs should collapse to one pattern");
        assert!(artifact.failures[0].contains("task not found: <id>"));
    }

    #[test]
    fn returns_empty_artifact_when_no_failures() {
        let entries = vec![
            make_tool_result("plan", true, "plan ok"),
            make_tool_result("cargo_test", true, "cargo_test ok"),
        ];

        let artifact = build_lessons_artifact(&entries);
        assert!(artifact.failures.is_empty());
        assert!(artifact.fixes.is_empty());
        assert!(artifact.summary.is_empty() || artifact.summary.contains("0 failures"));
    }
}
