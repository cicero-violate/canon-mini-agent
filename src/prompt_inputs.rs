use anyhow::{bail, Context, Result};
use canon_llm::{config::LlmEndpoint, tab_management::TabManagerHandle, ws_server::WsBridge};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::constants::{INVARIANTS_FILE, MASTER_PLAN_FILE, OBJECTIVES_FILE, SPEC_FILE};
use crate::objectives::read_objectives_filtered;
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

pub fn read_text_or_empty(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

pub fn read_required_text(path: impl AsRef<Path>, name: &str) -> Result<String> {
    std::fs::read_to_string(path.as_ref()).with_context(|| format!("failed to read {name}"))
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

pub(crate) fn sanitize_diagnostics_for_planner(raw_diagnostics_text: &str) -> String {
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
        return raw_diagnostics_text.to_string();
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
    let objectives_text = read_objectives_filtered(&workspace.join(OBJECTIVES_FILE));
    let invariants_text = read_text_or_empty(workspace.join(INVARIANTS_FILE));
    let violations_text = read_text_or_empty(violations_path);
    let raw_diagnostics_text = read_text_or_empty(diagnostics_path);
    let diagnostics_text = sanitize_diagnostics_for_planner(&raw_diagnostics_text);
    let plan_text = read_text_or_empty(master_plan_path);
    let plan_diff_text = plan_diff(last_plan_text, &plan_text, 400);
    PlannerInputs {
        summary_text,
        executor_diff_text,
        cargo_test_failures,
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
    Violations,
    Diagnostics,
    MasterPlan,
    Spec,
}

impl SingleRoleContext<'_> {
    pub fn read(&self, kind: SingleRoleRead) -> Result<String> {
        let text = match kind {
            SingleRoleRead::Objectives => {
                read_objectives_filtered(&self.workspace.join(OBJECTIVES_FILE))
            }
            SingleRoleRead::Invariants => read_text_or_empty(self.workspace.join(INVARIANTS_FILE)),
            SingleRoleRead::Violations => read_text_or_empty(self.violations_path),
            SingleRoleRead::Diagnostics => read_text_or_empty(self.diagnostics_path),
            SingleRoleRead::MasterPlan => read_text_or_empty(self.master_plan_path),
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
            let invariants = ctx.read(SingleRoleRead::Invariants)?;
            let objectives = ctx.read(SingleRoleRead::Objectives)?;
            let executor_diff_text = ctx.read_executor_diff(400);
            single_role_verifier_prompt(
                &inputs.primary_input,
                &objectives,
                &invariants,
                &executor_diff_text,
                cargo_test_failures,
            )
        }
        AgentPromptKind::Diagnostics => {
            let violations = ctx.read(SingleRoleRead::Violations)?;
            let objectives = ctx.read(SingleRoleRead::Objectives)?;
            single_role_diagnostics_prompt(&violations, &objectives, cargo_test_failures)
        }
        AgentPromptKind::Planner => {
            let violations = ctx.read(SingleRoleRead::Violations)?;
            let raw_diagnostics = ctx.read(SingleRoleRead::Diagnostics)?;
            let diagnostics = sanitize_diagnostics_for_planner(&raw_diagnostics);
            let objectives = ctx.read(SingleRoleRead::Objectives)?;
            let invariants = ctx.read(SingleRoleRead::Invariants)?;
            single_role_planner_prompt(
                &inputs.primary_input,
                &objectives,
                &invariants,
                &violations,
                &diagnostics,
                cargo_test_failures,
            )
        }
        AgentPromptKind::Executor => {
            let spec = ctx.read(SingleRoleRead::Spec)?;
            let master_plan = ctx.read(SingleRoleRead::MasterPlan)?;
            let violations = ctx.read(SingleRoleRead::Violations)?;
            let diagnostics = ctx.read(SingleRoleRead::Diagnostics)?;
            let invariants = ctx.read(SingleRoleRead::Invariants)?;
            single_role_executor_prompt(
                &spec,
                &master_plan,
                &violations,
                &diagnostics,
                &invariants,
            )
        }
        AgentPromptKind::Solo => {
            bail!("solo role is only supported in orchestration mode")
        }
    };
    Ok(prompt)
}

fn executor_diff_unavailable(reason: &str) -> String {
    format!("(executor diff unavailable: {reason})")
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
    let files: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            !line.starts_with("PLAN.json")
                && !line.starts_with("PLAN.md")
                && !line.starts_with("PLANS/")
                && *line != "VIOLATIONS.json"
                && *line != "DIAGNOSTICS.json"
        })
        .collect();
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
mod tests {
    use super::sanitize_diagnostics_for_planner;

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

        let sanitized = sanitize_diagnostics_for_planner(raw);
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

        let sanitized = sanitize_diagnostics_for_planner(raw);
        assert!(sanitized.contains("SOURCE-VALIDATED DIAGNOSTICS"));
        assert!(sanitized.contains("validated diagnostics"));
    }
}
