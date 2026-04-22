use crate::canonical_writer::CanonicalWriter;
use crate::llm_runtime::config::LlmEndpoint;
use anyhow::{anyhow, bail, Context, Result};
use ra_ap_syntax::{AstNode, Edition, SourceFile, SyntaxKind, SyntaxToken};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{hash_map::DefaultHasher, BTreeSet};
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::canon_tools_patch::apply_patch;
use crate::constants::{
    diagnostics_file, is_self_modification_mode, ISSUES_FILE, MASTER_PLAN_FILE,
    MAX_FULL_READ_LINES, MAX_SNIPPET, OBJECTIVES_FILE, SPEC_FILE, VIOLATIONS_FILE,
};
use crate::events::ControlEvent;
use crate::issues::{is_closed, Issue, IssuesFile};
use crate::logging::{
    append_orchestration_trace, log_action_event, log_action_result, log_error_event, now_ms,
};
use crate::objectives::filter_incomplete_objectives_json;
use crate::prompts::truncate;
use crate::tool_schema::{
    plan_set_plan_status_action_example, plan_set_task_status_action_example,
};

/// Return a human-readable type name for a JSON value (used in validation error messages).
fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Extract the first file path touched by the patch (*** Update File: / *** Add File:).
fn patch_first_file(patch: &str) -> Option<&str> {
    for line in patch.lines() {
        if let Some(rest) = line
            .strip_prefix("*** Update File:")
            .or_else(|| line.strip_prefix("*** Add File:"))
        {
            let path = rest.trim();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    None
}

fn patch_targets<'a>(patch: &'a str) -> Vec<&'a str> {
    patch
        .lines()
        .filter_map(|line| {
            line.strip_prefix("*** Update File:")
                .or_else(|| line.strip_prefix("*** Add File:"))
                .or_else(|| line.strip_prefix("*** Delete File:"))
                .or_else(|| line.strip_prefix("*** Move to:"))
        })
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect()
}

fn patch_targets_include_rust_sources(paths: &[&str]) -> bool {
    paths.iter().any(|path| path.ends_with(".rs"))
}

fn rust_patch_verification_flag_path() -> PathBuf {
    Path::new(crate::constants::agent_state_dir()).join("rust_patch_verification_requested.flag")
}

fn request_rust_patch_verification_if_needed(
    patch_targets: &[&str],
    mut writer: Option<&mut CanonicalWriter>,
) {
    if !patch_targets_include_rust_sources(patch_targets) {
        return;
    }
    if let Some(w) = writer.as_deref_mut() {
        w.apply(ControlEvent::RustPatchVerificationRequested { requested: true });
    }
    let flag_path = rust_patch_verification_flag_path();
    if let Some(parent) = flag_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(flag_path, b"requested\n");
}

fn fnv1a64(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn artifact_write_signature(parts: &[&str]) -> String {
    fnv1a64(&parts.join("\u{1f}"))
}

fn file_snapshot(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn restore_file_snapshot(path: &Path, snapshot: &Option<Vec<u8>>) -> Result<()> {
    if let Some(bytes) = snapshot {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn emit_workspace_artifact_effect(
    writer: &mut CanonicalWriter,
    requested: bool,
    artifact: &str,
    op: &str,
    target: &str,
    subject: &str,
    signature: &str,
) -> Result<()> {
    let effect = if requested {
        crate::events::EffectEvent::WorkspaceArtifactWriteRequested {
            artifact: artifact.to_string(),
            op: op.to_string(),
            target: target.to_string(),
            subject: subject.to_string(),
            signature: signature.to_string(),
        }
    } else {
        crate::events::EffectEvent::WorkspaceArtifactWriteApplied {
            artifact: artifact.to_string(),
            op: op.to_string(),
            target: target.to_string(),
            subject: subject.to_string(),
            signature: signature.to_string(),
        }
    };
    writer.try_record_effect(effect)
}

fn try_emit_workspace_artifact_effect(
    writer: &mut CanonicalWriter,
    requested: bool,
    artifact: &str,
    op: &str,
    target: &str,
    subject: &str,
    signature: &str,
) -> Result<()> {
    emit_workspace_artifact_effect(writer, requested, artifact, op, target, subject, signature)
        .map_err(|err| {
            anyhow!(
                "canonical effect append failed for {} {} {}: {err:#}",
                artifact,
                op,
                subject
            )
        })
}

fn record_effect_for_workspace(workspace: &Path, effect: crate::events::EffectEvent) -> Result<()> {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let state = crate::system_state::SystemState::new(&[], 0);
    let mut writer = crate::canonical_writer::CanonicalWriter::try_new(
        state,
        crate::tlog::Tlog::open(&tlog_path),
        workspace.to_path_buf(),
    )?;
    writer.try_record_effect(effect)
}

fn write_projection_with_workspace_effects(
    workspace: &Path,
    path: &Path,
    artifact: &str,
    subject: &str,
    contents: &str,
) -> Result<()> {
    let signature =
        artifact_write_signature(&[artifact, "write", subject, &contents.len().to_string()]);
    let target = path.to_string_lossy().into_owned();
    crate::logging::record_workspace_artifact_effect(
        workspace, true, artifact, "write", &target, subject, &signature,
    )?;
    let snapshot = file_snapshot(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, contents)?;
    std::fs::rename(&tmp_path, path)?;
    if let Err(err) = crate::logging::record_workspace_artifact_effect(
        workspace, false, artifact, "write", &target, subject, &signature,
    ) {
        restore_file_snapshot(path, &snapshot)?;
        return Err(err);
    }
    Ok(())
}

fn is_lane_plan(path: &str) -> bool {
    if !path.starts_with("PLANS/") && !path.starts_with("agent_state/") {
        return false;
    }
    let is_json = path.ends_with(".json");
    let is_md = path.ends_with(".md");
    if !is_json && !is_md {
        return false;
    }
    // Allow both legacy and instance-scoped lane plans:
    // - PLANS/executor-<id>.json
    // - PLANS/<instance>/executor-<id>.json
    // - agent_state/<instance>/executor-<id>.json
    // - legacy .md variants
    path.starts_with("PLANS/executor-")
        || path.starts_with("agent_state/executor-")
        || path.contains("/executor-")
}

fn is_src_path(path: &str) -> bool {
    is_named_dir_path(path, "src")
}

fn is_tests_path(path: &str) -> bool {
    is_named_dir_path(path, "tests")
}

fn is_named_dir_path(path: &str, dir: &str) -> bool {
    path == dir || path.starts_with(&format!("{dir}/"))
}

fn default_graph_out_dir(workspace: &Path, crate_name: &str) -> PathBuf {
    workspace
        .join("state")
        .join("reports_out")
        .join("crates")
        .join(crate_name)
}

fn report_crate_dir(out_dir: &Path, crate_name: &str) -> PathBuf {
    let name_matches = out_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == crate_name)
        .unwrap_or(false);
    let parent_is_crates = out_dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|n| n == "crates")
        .unwrap_or(false);
    if name_matches && parent_is_crates {
        out_dir.to_path_buf()
    } else {
        out_dir.join("crates").join(crate_name)
    }
}

fn graph_artifact_edges(workspace: &Path, crate_name: &str) -> Option<(u64, u64)> {
    let path = workspace
        .join("state")
        .join("graph")
        .join("index")
        .join("by_crate")
        .join(format!("{crate_name}.json"));
    let raw = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let call = value
        .get("call_edge_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cfg = value
        .get("cfg_edge_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some((call, cfg))
}

fn resolve_graph_crate_name(workspace: &Path, crate_name: &str) -> Option<String> {
    let primary = crate_name.to_string();
    let alt = crate_name.replace('-', "_");
    let candidates = if primary == alt {
        vec![primary]
    } else {
        vec![primary, alt]
    };
    for name in candidates {
        if let Some((call, cfg)) = graph_artifact_edges(workspace, &name) {
            if call > 0 || cfg > 0 {
                return Some(name);
            }
        }
    }
    None
}

fn ensure_graph_artifact(
    workspace: &Path,
    crate_name: &str,
    role: &str,
    step: usize,
) -> Result<String> {
    if let Some(name) = resolve_graph_crate_name(workspace, crate_name) {
        return Ok(name);
    }
    // Try rustc wrapper build to refresh artifacts.
    let build_cmd = format!("cargo build -p {crate_name}");
    eprintln!(
        "[{role}] step={} graph_artifact build cmd={build_cmd}",
        step
    );
    let (build_ok, build_out) =
        exec_run_command(workspace, &build_cmd, crate::constants::workspace())?;
    eprintln!(
        "[{role}] step={} graph_artifact build {} output_bytes={}",
        step,
        if build_ok { "ok" } else { "failed" },
        build_out.len()
    );
    if build_ok {
        if let Some(name) = resolve_graph_crate_name(workspace, crate_name) {
            return Ok(name);
        }
    }
    bail!(
        "graph artifact missing or lacks call/cfg edges; build the crate with rustc-wrapper enabled (e.g. `cargo build -p {crate_name}`) to generate a capture artifact"
    )
}

fn read_first_lines(path: &Path, max_lines: usize, max_bytes: usize) -> Result<String> {
    let content = ctx_read(path)?;
    let mut out = String::new();
    for (idx, line) in content.lines().enumerate() {
        if idx >= max_lines || out.len() >= max_bytes {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

fn read_json_report(path: &Path, max_bytes: usize) -> Result<String> {
    let content = ctx_read(path)?;
    let trimmed = truncate(&content, max_bytes);
    Ok(trimmed.to_string())
}

fn normalize_objective_id_for_match(value: &str) -> String {
    value
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
}

fn objective_id_matches(candidate: &str, requested: &str) -> bool {
    normalize_objective_id_for_match(candidate) == normalize_objective_id_for_match(requested)
}

fn objective_compared_ids(objectives: &[crate::objectives::Objective]) -> Vec<String> {
    objectives.iter().map(|obj| obj.id.clone()).collect()
}

fn objective_compared_normalized_ids(objectives: &[crate::objectives::Objective]) -> Vec<String> {
    objectives
        .iter()
        .map(|obj| normalize_objective_id_for_match(&obj.id))
        .collect()
}

fn objective_not_found_message(
    objectives: &[crate::objectives::Objective],
    requested: &str,
) -> String {
    let requested_id = normalize_objective_id_for_match(requested);
    let requested_raw = requested.to_string();
    let compared_ids = objective_compared_ids(objectives);
    let compared_normalized_ids = objective_compared_normalized_ids(objectives);
    format!(
        "objective not found: requested_raw={requested_raw:?}; requested_id={requested_id}; compared_ids={compared_ids:?}; compared_normalized_ids={compared_normalized_ids:?}"
    )
}

fn format_objective_already_exists_message(
    requested_raw: &str,
    requested_id: &str,
    compared_ids: &[String],
    compared_normalized_ids: &[String],
) -> String {
    format!(
        "objective id already exists: requested_raw={requested_raw:?}; requested_id={requested_id}; compared_ids={compared_ids:?}; compared_normalized_ids={compared_normalized_ids:?}"
    )
}

fn objective_already_exists_message(
    objectives: &[crate::objectives::Objective],
    requested: &str,
) -> String {
    let requested_id = normalize_objective_id_for_match(requested);
    let requested_raw = requested.to_string();
    let compared_ids = objective_compared_ids(objectives);
    let compared_normalized_ids = objective_compared_normalized_ids(objectives);
    format_objective_already_exists_message(
        &requested_raw,
        &requested_id,
        &compared_ids,
        &compared_normalized_ids,
    )
}

fn log_objective_operation_context(
    op: &str,
    outcome: &str,
    requested: Option<&str>,
    objectives: &[crate::objectives::Objective],
) {
    let requested_raw = requested.map(str::to_string);
    let requested_id = requested.map(normalize_objective_id_for_match);
    append_orchestration_trace(
        "objective_operation_context",
        json!({
            "operation": op,
            "outcome": outcome,
            "requested_raw": requested_raw,
            "requested_id": requested_id,
            "compared_ids": objective_compared_ids(objectives),
            "compared_normalized_ids": objective_compared_normalized_ids(objectives),
        }),
    );
}

fn sort_objectives_for_view(file: &mut crate::objectives::ObjectivesFile, include_done: bool) {
    file.objectives.sort_by(|a, b| {
        let rank = |status: &str| match status.trim().to_lowercase().as_str() {
            "active" => 0,
            "ready" => 1,
            "in_progress" => 2,
            "blocked" => 3,
            "done" | "complete" | "completed" => 4,
            _ => 5,
        };
        rank(&a.status)
            .cmp(&rank(&b.status))
            .then_with(|| a.id.cmp(&b.id))
    });
    if !include_done {
        file.objectives = file
            .objectives
            .drain(..)
            .filter(|obj| !crate::objectives::is_completed(obj))
            .collect();
    }
}

fn parse_objectives_file_strict(raw: &str) -> Result<crate::objectives::ObjectivesFile> {
    let file: crate::objectives::ObjectivesFile =
        serde_json::from_str(raw).map_err(|e| anyhow!("failed to parse OBJECTIVES.json: {e}"))?;
    validate_unique_objective_ids(&file)?;
    Ok(file)
}

fn parse_objectives_file_or_default(raw: &str) -> crate::objectives::ObjectivesFile {
    serde_json::from_str(raw).unwrap_or_default()
}

fn validate_unique_objective_ids(file: &crate::objectives::ObjectivesFile) -> Result<()> {
    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for objective in &file.objectives {
        let canonical = objective.id.trim().to_ascii_lowercase();
        if canonical.is_empty() {
            bail!("objective.id must be non-empty");
        }
        if let Some(existing) = seen.get(&canonical) {
            bail!(
                "duplicate objective id in OBJECTIVES.json: '{}' conflicts with '{}'",
                objective.id,
                existing
            );
        }
        seen.insert(canonical, objective.id.clone());
    }
    Ok(())
}

fn write_objectives_file(
    workspace: &Path,
    path: &Path,
    file: &crate::objectives::ObjectivesFile,
    mut writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    validate_unique_objective_ids(file)?;
    let contents = serde_json::to_string_pretty(file)?;
    if let Some(writer_ref) = writer.as_deref_mut() {
        writer_ref.apply(crate::events::ControlEvent::ObjectivesReplaced {
            hash: crate::objectives::objectives_hash(&contents),
            contents: contents.clone(),
        });
        crate::objectives::persist_objectives_projection(
            workspace,
            &contents,
            "objectives_projection_from_canonical_state",
        )?;
        return Ok((false, "objectives write ok".to_string()));
    }
    if let Some(agent_state_dir) = path.parent() {
        if agent_state_dir
            .file_name()
            .is_some_and(|name| name == "agent_state")
        {
            if let Some(workspace) = agent_state_dir.parent() {
                write_projection_with_workspace_effects(
                    workspace,
                    path,
                    OBJECTIVES_FILE,
                    "objectives_write",
                    &contents,
                )?;
                return Ok((false, "objectives write ok".to_string()));
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, &contents)?;
    std::fs::rename(&tmp_path, path)?;
    Ok((false, "objectives write ok".to_string()))
}

fn objective_id_from_action<'a>(action: &'a Value, op_name: &str) -> Result<&'a str> {
    action
        .get("objective_id")
        .or_else(|| action.get("id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("objectives {op_name} missing objective_id"))
}

fn log_objective_not_found_and_bail(
    op: &str,
    objective_id: &str,
    objectives: &[crate::objectives::Objective],
) -> Result<(bool, String)> {
    log_objective_operation_context(op, "not_found", Some(objective_id), objectives);
    bail!("{}", objective_not_found_message(objectives, objective_id));
}

fn handle_objectives_read(raw: &str, include_done: bool) -> Result<(bool, String)> {
    if include_done {
        return Ok((false, raw.to_string()));
    }
    let filtered = filter_incomplete_objectives_json(raw).unwrap_or(raw.to_string());
    Ok((false, filtered))
}

fn handle_objectives_sorted_view(raw: &str, include_done: bool) -> Result<(bool, String)> {
    let mut file = parse_objectives_file_strict(raw)?;
    sort_objectives_for_view(&mut file, include_done);
    Ok((
        false,
        serde_json::to_string_pretty(&file).unwrap_or(raw.to_string()),
    ))
}

fn handle_objectives_action_with_writer(
    workspace: &Path,
    action: &Value,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let op_raw = action
        .get("op")
        .and_then(|v| v.as_str())
        .or_else(|| action.get("operation").and_then(|v| v.as_str()))
        .unwrap_or("read");
    let include_done = action
        .get("include_done")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let path = workspace.join(OBJECTIVES_FILE);
    let raw = writer
        .as_deref()
        .map(|w| w.state().objectives_json.clone())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| crate::objectives::load_canonical_objectives_json(workspace))
        .unwrap_or_else(|| crate::objectives::load_runtime_objectives_json(workspace));
    if raw.trim().is_empty() && op_raw == "read" {
        return Ok((false, "(no objectives)".to_string()));
    }

    match op_raw {
        "read" => handle_objectives_read(&raw, include_done),
        "sorted_view" => handle_objectives_sorted_view(&raw, include_done),
        "create_objective" => handle_objectives_create_objective(workspace, action, &path, &raw, writer),
        "update_objective" => handle_objectives_update_objective(workspace, action, &path, &raw, writer),
        "delete_objective" => handle_objectives_delete_objective(workspace, action, &path, &raw, writer),
        "set_status" => handle_objectives_set_status(workspace, action, &path, &raw, writer),
        "replace_objectives" | "replace" => {
            handle_objectives_replace_objectives(workspace, action, &path, &raw, writer)
        }
        _ => bail!("unknown objectives op: {op_raw}"),
    }
}

fn handle_objectives_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    handle_objectives_action_with_writer(workspace, action, None)
}

fn handle_objectives_create_objective(
    workspace: &Path,
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let objective_val = action
        .get("objective")
        .ok_or_else(|| anyhow!("objectives create_objective missing objective"))?;

    // Pre-check: validate that array fields are actually arrays before serde deserialization.
    // serde reports "invalid type: string, expected a sequence" which doesn't name the field.
    if let Some(obj) = objective_val.as_object() {
        for field in &["verification", "success_criteria"] {
            if let Some(v) = obj.get(*field) {
                if !v.is_array() {
                    bail!(
                        "invalid objective payload: field '{}' must be an array, got {}",
                        field,
                        value_type_name(v)
                    );
                }
            }
        }
    }

    let mut file = parse_objectives_file_strict(raw)?;
    let objective: crate::objectives::Objective = serde_json::from_value(objective_val.clone())
        .map_err(|e| anyhow!("invalid objective payload: {e}"))?;
    if objective.id.trim().is_empty() {
        bail!("objective.id must be non-empty");
    }
    log_objective_operation_context(
        "create_objective",
        "attempt",
        Some(&objective.id),
        &file.objectives,
    );
    if file
        .objectives
        .iter()
        .any(|o| objective_id_matches(&o.id, &objective.id))
    {
        log_objective_operation_context(
            "create_objective",
            "duplicate",
            Some(&objective.id),
            &file.objectives,
        );
        bail!(
            "{}",
            objective_already_exists_message(&file.objectives, &objective.id)
        );
    }
    file.objectives.push(objective);
    let created_id = file.objectives.last().map(|obj| obj.id.as_str());
    log_objective_operation_context("create_objective", "success", created_id, &file.objectives);
    write_objectives_file(workspace, path, &file, writer)
        .map(|_| (false, "objectives create_objective ok".to_string()))
}

fn handle_objectives_update_objective(
    workspace: &Path,
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let objective_id = objective_id_from_action(action, "update_objective")?;
    let updates = action
        .get("updates")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("objectives update_objective missing updates object. Required schema: {{\"op\":\"update_objective\",\"objective_id\":\"<id>\",\"updates\":{{\"title\":\"<title>\",\"status\":\"<status>\"}}}}"))?;
    let mut file = parse_objectives_file_strict(raw)?;
    log_objective_operation_context(
        "update_objective",
        "attempt",
        Some(objective_id),
        &file.objectives,
    );
    let mut found = false;
    for obj in file.objectives.iter_mut() {
        if objective_id_matches(&obj.id, objective_id) {
            let mut value = serde_json::to_value(obj.clone())?;
            if let Some(map) = value.as_object_mut() {
                for (k, v) in updates {
                    map.insert(k.clone(), v.clone());
                }
            }
            *obj = serde_json::from_value(value)?;
            found = true;
            break;
        }
    }
    if !found {
        return log_objective_not_found_and_bail(
            "update_objective",
            objective_id,
            &file.objectives,
        );
    }
    log_objective_operation_context(
        "update_objective",
        "success",
        Some(objective_id),
        &file.objectives,
    );
    write_objectives_file(workspace, path, &file, writer)
        .map(|_| (false, "objectives update_objective ok".to_string()))
}

fn handle_objectives_delete_objective(
    workspace: &Path,
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let objective_id = objective_id_from_action(action, "delete_objective")?;
    let mut file = parse_objectives_file_or_default(raw);
    log_objective_operation_context(
        "delete_objective",
        "attempt",
        Some(objective_id),
        &file.objectives,
    );
    let before = file.objectives.len();
    file.objectives
        .retain(|obj| !objective_id_matches(&obj.id, objective_id));
    if file.objectives.len() == before {
        return log_objective_not_found_and_bail(
            "delete_objective",
            objective_id,
            &file.objectives,
        );
    }
    log_objective_operation_context(
        "delete_objective",
        "success",
        Some(objective_id),
        &file.objectives,
    );
    write_objectives_file(workspace, path, &file, writer)
        .map(|_| (false, "objectives delete_objective ok".to_string()))
}

fn handle_objectives_set_status(
    workspace: &Path,
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let objective_id = objective_id_from_action(action, "set_status")?;
    let status = action
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("objectives set_status missing status"))?;
    let mut file = parse_objectives_file_or_default(raw);
    log_objective_operation_context(
        "set_status",
        "attempt",
        Some(objective_id),
        &file.objectives,
    );
    let mut found = false;
    for obj in file.objectives.iter_mut() {
        if objective_id_matches(&obj.id, objective_id) {
            obj.status = status.to_string();
            found = true;
            break;
        }
    }
    if !found {
        return log_objective_not_found_and_bail("set_status", objective_id, &file.objectives);
    }
    log_objective_operation_context(
        "set_status",
        "success",
        Some(objective_id),
        &file.objectives,
    );
    write_objectives_file(workspace, path, &file, writer)
        .map(|_| (false, "objectives set_status ok".to_string()))
}

fn handle_objectives_replace_objectives(
    workspace: &Path,
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let mut file = parse_objectives_file_or_default(raw);
    if let Some(obj_value) = action.get("objectives") {
        if obj_value.is_array() {
            let objectives: Vec<crate::objectives::Objective> =
                serde_json::from_value(obj_value.clone())
                    .map_err(|e| anyhow!("invalid objectives array: {e}"))?;
            file.objectives = objectives;
        } else if obj_value.is_object() {
            file = serde_json::from_value(obj_value.clone())
                .map_err(|e| anyhow!("invalid objectives file payload: {e}"))?;
        } else {
            bail!("objectives replace_objectives requires objectives array or object");
        }
    } else {
        bail!("objectives replace_objectives missing objectives");
    }
    write_objectives_file(workspace, path, &file, writer)
        .map(|_| (false, "objectives replace_objectives ok".to_string()))
}

fn load_violations(path: &Path) -> Result<crate::reports::ViolationsReport> {
    let raw = fs::read_to_string(path).unwrap_or_default();
    if raw.trim().is_empty() {
        if let Some(report) = load_violations_from_tlog(path) {
            return Ok(report);
        }
        return Ok(crate::reports::ViolationsReport {
            status: "ok".to_string(),
            summary: String::new(),
            violations: vec![],
        });
    }
    serde_json::from_str(&raw).map_err(|e| anyhow!("VIOLATIONS.json parse error: {e}"))
}

fn load_violations_from_tlog(path: &Path) -> Option<crate::reports::ViolationsReport> {
    let tlog_path = path
        .parent()
        .map(|dir| dir.join("tlog.ndjson"))
        .unwrap_or_else(|| Path::new(crate::constants::agent_state_dir()).join("tlog.ndjson"));
    let records = crate::tlog::Tlog::read_records(&tlog_path).ok()?;
    let mut latest: Option<(u64, crate::reports::ViolationsReport)> = None;
    for record in records {
        let crate::events::Event::Effect {
            event: crate::events::EffectEvent::ViolationsReportRecorded { report },
        } = record.event
        else {
            continue;
        };
        let replace = match latest.as_ref() {
            None => true,
            Some((seq, _)) => record.seq >= *seq,
        };
        if replace {
            latest = Some((record.seq, report));
        }
    }
    latest.map(|(_, report)| report)
}

fn save_violations(
    path: &Path,
    report: &crate::reports::ViolationsReport,
    mut writer: Option<&mut CanonicalWriter>,
    op: &str,
    subject: &str,
) -> Result<()> {
    let json = serde_json::to_string_pretty(report)?;
    let effect = crate::events::EffectEvent::ViolationsReportRecorded {
        report: report.clone(),
    };
    if let Some(writer_ref) = writer.as_deref_mut() {
        writer_ref.try_record_effect(effect)?;
    } else {
        crate::logging::record_effect_for_workspace(
            std::path::Path::new(crate::constants::workspace()),
            effect,
        )?;
    }
    crate::logging::write_projection_with_artifact_effects(
        std::path::Path::new(crate::constants::workspace()),
        path,
        VIOLATIONS_FILE,
        op,
        subject,
        &json,
    )
    .map_err(|e| anyhow!("failed to write VIOLATIONS.json: {e}"))
}

fn handle_violation_action(
    mut writer: Option<&mut CanonicalWriter>,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    use crate::reports::{Violation, ViolationsReport};
    let op_raw = action.get("op").and_then(|v| v.as_str()).unwrap_or("read");
    let path = workspace.join(VIOLATIONS_FILE);

    match op_raw {
        "read" => {
            let report = load_violations(&path)?;
            Ok((false, serde_json::to_string_pretty(&report)?))
        }
        "upsert" => {
            let lease = validate_evidence_lease(action)?;
            // Add or replace a violation by id.
            let v_val = action
                .get("violation")
                .ok_or_else(|| anyhow!("violation upsert requires a 'violation' object"))?;
            let mut v: Violation = serde_json::from_value(v_val.clone())
                .map_err(|e| anyhow!("invalid violation payload: {e}"))?;
            if v.id.trim().is_empty() {
                bail!("violation.id must be non-empty");
            }
            apply_violation_freshness(&mut v, &lease);
            let mut report = load_violations(&path)?;
            if let Some(existing) = report.violations.iter_mut().find(|x| x.id == v.id) {
                *existing = v.clone();
                save_violations(&path, &report, writer.as_deref_mut(), "upsert", &v.id)?;
                Ok((false, format!("violation upsert ok — updated `{}`", v.id)))
            } else {
                report.violations.push(v.clone());
                save_violations(&path, &report, writer.as_deref_mut(), "upsert", &v.id)?;
                Ok((false, format!("violation upsert ok — added `{}`", v.id)))
            }
        }
        "resolve" => {
            validate_evidence_lease(action)?;
            let vid = action
                .get("violation_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("violation resolve requires 'violation_id'"))?;
            let mut report = load_violations(&path)?;
            let before = report.violations.len();
            report.violations.retain(|v| v.id != vid);
            if report.violations.len() == before {
                bail!("violation not found: {vid}");
            }
            if report.violations.is_empty() {
                report.status = "ok".to_string();
            }
            save_violations(&path, &report, writer.as_deref_mut(), "resolve", vid)?;
            Ok((false, format!("violation resolve ok — removed `{vid}`")))
        }
        "set_status" => {
            validate_evidence_lease(action)?;
            let status = action
                .get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("violation set_status requires 'status'"))?;
            let mut report = load_violations(&path)?;
            report.status = status.to_string();
            if let Some(s) = action.get("summary").and_then(|v| v.as_str()) {
                report.summary = s.to_string();
            }
            save_violations(&path, &report, writer.as_deref_mut(), "set_status", status)?;
            Ok((
                false,
                format!("violation set_status ok — status=`{status}`"),
            ))
        }
        "replace" => {
            let lease = validate_evidence_lease(action)?;
            let rep_val = action
                .get("report")
                .ok_or_else(|| anyhow!("violation replace requires a 'report' object"))?;
            let mut report: ViolationsReport = serde_json::from_value(rep_val.clone())
                .map_err(|e| anyhow!("invalid ViolationsReport payload: {e}"))?;
            report.violations.retain(violation_is_fresh);
            for violation in &mut report.violations {
                apply_violation_freshness(violation, &lease);
            }
            save_violations(&path, &report, writer.as_deref_mut(), "replace", "report")?;
            Ok((
                false,
                format!(
                    "violation replace ok — {} violation(s)",
                    report.violations.len()
                ),
            ))
        }
        _ => bail!(
            "unknown violation op '{op_raw}' — use: read | upsert | resolve | set_status | replace"
        ),
    }
}

fn handle_issue_action(
    mut writer: Option<&mut CanonicalWriter>,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    if let Err(err) = crate::issues::sweep_stale_issues(workspace) {
        eprintln!("[issue] stale sweep failed: {err:#}");
    }
    let op_raw = action.get("op").and_then(|v| v.as_str()).unwrap_or("read");
    let path = workspace.join(ISSUES_FILE);
    let raw = serde_json::to_string_pretty(&crate::issues::load_issues_file(workspace))
        .unwrap_or_default();
    match op_raw {
        "read" => read_open_issues(&raw),
        "create" => create_issue(action, &path, &raw, writer.as_deref_mut()),
        "update" => update_issue(action, &path, &raw, writer.as_deref_mut()),
        "delete" => delete_issue(action, &path, &raw, writer.as_deref_mut()),
        "set_status" => set_issue_status(action, &path, &raw, writer.as_deref_mut()),
        "upsert" => upsert_issue(action, &path, &raw, writer.as_deref_mut()),
        "resolve" => resolve_issue(action, &path, &raw, writer.as_deref_mut()),
        _ => {
            bail!(
                "unknown issue op '{op_raw}' — use read | create | update | delete | set_status | upsert | resolve"
            )
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EvidenceReceipt {
    id: String,
    ts_ms: u64,
    actor: String,
    step: usize,
    action: String,
    path: Option<String>,
    abs_path: Option<String>,
    meta: Value,
    output_hash: String,
}

#[derive(Debug, Clone)]
struct EvidenceLease {
    receipt_ids: Vec<String>,
    validated_from: Vec<String>,
    evidence_hashes: Vec<String>,
    last_validated_ms: u64,
}

fn evidence_receipts_path() -> PathBuf {
    Path::new(crate::constants::agent_state_dir()).join("evidence_receipts.jsonl")
}

fn stable_hash_hex(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn append_evidence_receipt(
    role: &str,
    step: usize,
    action: &str,
    rel_path: Option<&str>,
    abs_path: Option<PathBuf>,
    meta: Value,
    output: &str,
) -> Result<String> {
    let ts_ms = now_ms();
    let id = format!("rcpt-{ts_ms}-{role}-{step}-{action}");
    let receipt = build_evidence_receipt(
        &id,
        ts_ms,
        role,
        step,
        action,
        rel_path,
        abs_path,
        meta,
        output,
    );
    let path = evidence_receipts_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", serde_json::to_string(&receipt)?)?;
    Ok(id)
}

fn build_evidence_receipt(
    id: &str,
    ts_ms: u64,
    role: &str,
    step: usize,
    action: &str,
    rel_path: Option<&str>,
    abs_path: Option<PathBuf>,
    meta: Value,
    output: &str,
) -> EvidenceReceipt {
    EvidenceReceipt {
        id: id.to_string(),
        ts_ms,
        actor: role.to_string(),
        step,
        action: action.to_string(),
        path: rel_path.map(|s| s.to_string()),
        abs_path: abs_path.map(|p| p.display().to_string()),
        meta,
        output_hash: stable_hash_hex(output),
    }
}

fn format_output_with_evidence_receipt(
    prefix: &str,
    out: &str,
    receipt_id: Option<&str>,
) -> String {
    match receipt_id {
        Some(receipt_id) => format!("{prefix}:\nEvidence receipt: {receipt_id}\n{out}",),
        None => format!("{prefix}:\n{out}"),
    }
}

fn validate_evidence_lease(action: &Value) -> Result<EvidenceLease> {
    let receipt_ids = action
        .get("evidence_receipts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("authoritative mutation requires non-empty 'evidence_receipts'"))?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect::<Vec<_>>();
    if receipt_ids.is_empty() {
        bail!("authoritative mutation requires non-empty 'evidence_receipts'");
    }
    let raw = fs::read_to_string(evidence_receipts_path()).unwrap_or_default();
    let mut validated_from = Vec::new();
    let mut evidence_hashes = Vec::new();
    let mut last_validated_ms = 0u64;
    for receipt_id in &receipt_ids {
        let maybe_receipt = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<EvidenceReceipt>(line).ok())
            .find(|receipt| &receipt.id == receipt_id);
        let Some(receipt) = maybe_receipt else {
            bail!("evidence receipt not found: {receipt_id}");
        };
        if now_ms().saturating_sub(receipt.ts_ms) > 15 * 60 * 1000 {
            bail!("evidence receipt is stale: {receipt_id}");
        }
        if let Some(path) = receipt.path.or(receipt.abs_path) {
            validated_from.push(path);
        }
        evidence_hashes.push(receipt.output_hash);
        last_validated_ms = last_validated_ms.max(receipt.ts_ms);
    }
    validated_from.sort();
    validated_from.dedup();
    evidence_hashes.sort();
    evidence_hashes.dedup();
    Ok(EvidenceLease {
        receipt_ids,
        validated_from,
        evidence_hashes,
        last_validated_ms,
    })
}

fn apply_issue_freshness(issue: &mut Issue, lease: &EvidenceLease) {
    issue.freshness_status = "fresh".to_string();
    issue.stale_reason.clear();
    issue.last_validated_ms = lease.last_validated_ms;
    issue.validated_from = lease.validated_from.clone();
    issue.evidence_receipts = lease.receipt_ids.clone();
    issue.evidence_hashes = lease.evidence_hashes.clone();
}

fn violation_is_fresh(violation: &crate::reports::Violation) -> bool {
    match violation
        .freshness_status
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "fresh" => true,
        "stale" | "unknown" => false,
        _ => violation.last_validated_ms > 0,
    }
}

fn apply_violation_freshness(violation: &mut crate::reports::Violation, lease: &EvidenceLease) {
    violation.freshness_status = "fresh".to_string();
    violation.stale_reason.clear();
    violation.last_validated_ms = lease.last_validated_ms;
    violation.validated_from = lease.validated_from.clone();
    violation.evidence_receipts = lease.receipt_ids.clone();
    violation.evidence_hashes = lease.evidence_hashes.clone();
}

fn read_open_issues(raw: &str) -> Result<(bool, String)> {
    if raw.trim().is_empty() {
        return Ok((false, "(no open issues)".to_string()));
    }
    let mut file: IssuesFile = serde_json::from_str(raw).unwrap_or_default();
    file.issues.retain(|i| !is_closed(i));
    file.issues.retain(crate::issues::issue_is_fresh);
    if file.issues.is_empty() {
        return Ok((false, "(no open issues)".to_string()));
    }
    Ok((
        false,
        serde_json::to_string_pretty(&file).unwrap_or(raw.to_string()),
    ))
}

fn create_issue(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let lease = validate_evidence_lease(action)?;
    let issue_val = action
        .get("issue")
        .ok_or_else(|| anyhow!("issue create missing 'issue' field"))?;

    let mut file = parse_issues_file_allow_empty(raw)?;
    let mut issue = parse_issue_payload(issue_val)?;
    apply_issue_freshness(&mut issue, &lease);
    let issue_id = issue.id.clone();
    if file.issues.iter().any(|i| i.id == issue_id) {
        bail!("issue id already exists: {}", issue_id);
    }
    file.issues.push(issue);
    write_issues_file(path, &mut file, writer, "create", &issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, "issue create ok".to_string()))
}

fn parse_issue_payload(issue_val: &Value) -> Result<Issue> {
    // Pre-check: collect all missing required string fields before serde attempts deserialization.
    // serde only reports the first missing field; this lists them all so the LLM can fix in one shot.
    if let Some(obj) = issue_val.as_object() {
        let required_string_fields = ["id", "title", "status", "priority"];
        let missing: Vec<&str> = required_string_fields
            .iter()
            .copied()
            .filter(|&field| {
                obj.get(field)
                    .map(|v| v.as_str().map(|s| s.trim().is_empty()).unwrap_or(true))
                    .unwrap_or(true)
            })
            .collect();
        if !missing.is_empty() {
            bail!(
                "invalid issue payload: missing required fields: {}. Required: {{\"id\":\"<id>\",\"title\":\"<title>\",\"status\":\"open\",\"priority\":\"medium\",\"kind\":\"<kind>\",\"description\":\"<description>\"}}",
                missing.join(", ")
            );
        }
        // Pre-check: field type validation — ensure string fields are not wrong types.
        let string_fields = [
            "id",
            "title",
            "status",
            "priority",
            "kind",
            "description",
            "location",
            "discovered_by",
        ];
        for field in &string_fields {
            if let Some(v) = obj.get(*field) {
                if !v.is_string() && !v.is_null() {
                    bail!(
                        "invalid issue payload: field '{}' must be a string, got {}",
                        field,
                        value_type_name(v)
                    );
                }
            }
        }
    } else if !issue_val.is_null() {
        bail!(
            "invalid issue payload: expected an object, got {}",
            value_type_name(issue_val)
        );
    }

    let issue: Issue = serde_json::from_value(issue_val.clone())
        .map_err(|e| anyhow!("invalid issue payload: {e}"))?;
    if issue.id.trim().is_empty() {
        bail!("issue.id must be non-empty");
    }
    Ok(issue)
}

fn upsert_issue(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let lease = validate_evidence_lease(action)?;
    let issue_val = action
        .get("issue")
        .ok_or_else(|| anyhow!("issue upsert missing 'issue' field"))?;
    let mut file = parse_issues_file_allow_empty(raw)?;
    let mut issue = parse_issue_payload(issue_val)?;
    if let Some(issue_id) = action.get("issue_id").and_then(|v| v.as_str()) {
        if issue.id != issue_id {
            bail!(
                "issue upsert mismatch: issue_id '{}' does not match issue.id '{}'",
                issue_id,
                issue.id
            );
        }
    }
    apply_issue_freshness(&mut issue, &lease);
    let issue_id = issue.id.clone();
    let outcome = if let Some(existing) = file.issues.iter_mut().find(|i| i.id == issue_id) {
        *existing = issue;
        "updated"
    } else {
        file.issues.push(issue);
        "added"
    };
    write_issues_file(path, &mut file, writer, "upsert", &issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, format!("issue upsert ok — {outcome} `{issue_id}`")))
}

fn resolve_issue(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let lease = validate_evidence_lease(action)?;
    let issue_id = action
        .get("issue_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            action
                .get("issue")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
        })
        .ok_or_else(|| anyhow!("issue resolve missing 'issue_id'"))?;
    let mut file = parse_issues_file_required(raw)?;
    let issue = find_issue_mut(&mut file, issue_id)?;
    issue.status = "resolved".to_string();
    apply_issue_freshness(issue, &lease);
    write_issues_file(path, &mut file, writer, "resolve", issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, format!("issue resolve ok — `{issue_id}`")))
}

fn update_issue(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let lease = validate_evidence_lease(action)?;
    let issue_id = action
        .get("issue_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("issue update missing 'issue_id'"))?;
    let updates = action
        .get("updates")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("issue update missing 'updates' object"))?;
    let mut file = parse_issues_file_required(raw)?;
    let issue = find_issue_mut(&mut file, issue_id)?;
    let mut value = serde_json::to_value(issue.clone())?;
    if let Some(map) = value.as_object_mut() {
        for (k, v) in updates {
            map.insert(k.clone(), v.clone());
        }
    }
    *issue = serde_json::from_value(value)?;
    apply_issue_freshness(issue, &lease);
    write_issues_file(path, &mut file, writer, "update", issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, "issue update ok".to_string()))
}

fn delete_issue(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let issue_id = action
        .get("issue_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("issue delete missing 'issue_id'"))?;
    let mut file: IssuesFile = serde_json::from_str(raw).unwrap_or_default();
    let before = file.issues.len();
    file.issues.retain(|i| i.id != issue_id);
    if file.issues.len() == before {
        bail!("issue not found: {issue_id}");
    }
    write_issues_file(path, &mut file, writer, "delete", issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, "issue delete ok".to_string()))
}

fn set_issue_status(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let lease = validate_evidence_lease(action)?;
    let issue_id = action
        .get("issue_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("issue set_status missing 'issue_id'"))?;
    let status = action
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("issue set_status missing 'status'"))?;
    let mut file = parse_issues_file_required(raw)?;
    let issue = find_issue_mut(&mut file, issue_id)?;
    issue.status = status.to_string();
    apply_issue_freshness(issue, &lease);
    write_issues_file(path, &mut file, writer, "set_status", issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, "issue set_status ok".to_string()))
}

fn queue_diagnostics_reconciliation() {
    // Wake signals are canonicalized through ControlEvent::WakeSignalQueued.
    // Legacy wakeup_*.flag projections are retired.
}

fn parse_issues_file_allow_empty(raw: &str) -> Result<IssuesFile> {
    if raw.trim().is_empty() {
        Ok(IssuesFile::default())
    } else {
        parse_issues_file_required(raw)
    }
}

fn parse_issues_file_required(raw: &str) -> Result<IssuesFile> {
    serde_json::from_str(raw).map_err(|e| anyhow!("failed to parse ISSUES.json: {e}"))
}

fn find_issue_mut<'a>(file: &'a mut IssuesFile, issue_id: &str) -> Result<&'a mut Issue> {
    let found = file.issues.iter_mut().find(|i| i.id == issue_id);
    let Some(issue) = found else {
        bail!("issue not found: {issue_id}");
    };
    Ok(issue)
}

fn write_issues_file(
    path: &Path,
    file: &mut IssuesFile,
    writer: Option<&mut CanonicalWriter>,
    op: &str,
    subject: &str,
) -> Result<()> {
    crate::issues::rescore_all(file);
    let workspace = path
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| std::path::Path::new(crate::constants::workspace()));
    crate::issues::persist_issues_projection_with_writer(workspace, file, writer, subject)
        .map_err(|e| anyhow!("failed to write ISSUES.json via {op}: {e}"))
}

fn handle_plan_sorted_view_action(workspace: &Path) -> Result<(bool, String)> {
    let (obj, tasks, edges) = load_plan_components(workspace)?;
    let task_map = build_task_map(&tasks);
    let order = topo_sort_plan(&task_map, &edges)?;
    let ordered_tasks = collect_ordered_tasks(&task_map, &order);
    let rendered = render_plan_sorted_view_output(&obj, order, ordered_tasks, &edges)?;
    Ok((false, rendered))
}

fn load_plan_components(
    workspace: &Path,
) -> Result<(serde_json::Map<String, Value>, Vec<Value>, Vec<Value>)> {
    let plan_path = workspace.join(MASTER_PLAN_FILE);
    let plan = load_or_init_plan(&plan_path)?;
    let obj = plan
        .as_object()
        .ok_or_else(|| anyhow!("PLAN.json must be a JSON object"))?;
    let tasks = obj
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    let edges = obj
        .get("dag")
        .and_then(|v| v.as_object())
        .and_then(|d| d.get("edges"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
    ensure_dag(tasks, edges)?;
    Ok((obj.clone(), tasks.clone(), edges.clone()))
}

fn build_task_map(tasks: &[Value]) -> std::collections::HashMap<String, Value> {
    let mut task_map = std::collections::HashMap::new();
    for task in tasks {
        if let Some(id) = task.get("id").and_then(|v| v.as_str()) {
            task_map.insert(id.to_string(), task.clone());
        }
    }
    task_map
}

fn topo_sort_plan(
    task_map: &std::collections::HashMap<String, Value>,
    edges: &[Value],
) -> Result<Vec<String>> {
    let (mut indegree, adj) = build_plan_sorted_graph_state(task_map, edges)?;
    let mut ready: BTreeSet<String> = indegree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut order = Vec::new();
    while let Some(id) = take_next_ready_plan_node(&mut ready) {
        order.push(id.clone());
        drain_plan_sorted_successors(&adj, &id, &mut indegree, &mut ready);
    }
    Ok(order)
}

fn collect_ordered_tasks(
    task_map: &std::collections::HashMap<String, Value>,
    order: &[String],
) -> Vec<Value> {
    let mut ordered = Vec::new();
    for id in order {
        if let Some(task) = task_map.get(id) {
            ordered.push(task.clone());
        }
    }
    ordered
}

fn build_plan_sorted_graph_state(
    task_map: &std::collections::HashMap<String, Value>,
    edges: &[Value],
) -> Result<(
    std::collections::HashMap<String, usize>,
    std::collections::HashMap<String, BTreeSet<String>>,
)> {
    let mut indegree: std::collections::HashMap<String, usize> =
        task_map.keys().map(|k| (k.clone(), 0)).collect();
    let mut adj: std::collections::HashMap<String, BTreeSet<String>> =
        std::collections::HashMap::new();
    for id in task_map.keys() {
        adj.insert(id.clone(), BTreeSet::new());
    }
    for edge in edges {
        let from = edge.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let to = edge.get("to").and_then(|v| v.as_str()).unwrap_or("");
        if from.is_empty() || to.is_empty() {
            bail!(format!(
                "plan edge missing from/to; candidate edge: {}",
                serde_json::to_string(edge).unwrap_or_else(|_| "<invalid edge json>".to_string())
            ));
        }
        insert_plan_edge_adjacency(&mut adj, from, to);
        increment_plan_edge_indegree(&mut indegree, to);
    }
    Ok((indegree, adj))
}

fn render_plan_sorted_view_output(
    obj: &serde_json::Map<String, Value>,
    order: Vec<String>,
    ordered_tasks: Vec<Value>,
    edges: &[Value],
) -> Result<String> {
    let mut output = serde_json::Map::new();
    if let Some(version) = obj.get("version") {
        output.insert("version".to_string(), version.clone());
    }
    if let Some(status) = obj.get("status") {
        output.insert("status".to_string(), status.clone());
    }
    output.insert(
        "order".to_string(),
        Value::Array(order.into_iter().map(Value::String).collect()),
    );
    output.insert("tasks".to_string(), Value::Array(ordered_tasks));
    output.insert("edges".to_string(), Value::Array(edges.to_vec()));
    Ok(serde_json::to_string_pretty(&Value::Object(output))?)
}

fn insert_plan_edge_adjacency(
    adj: &mut std::collections::HashMap<String, BTreeSet<String>>,
    from: &str,
    to: &str,
) {
    if let Some(nexts) = adj.get_mut(from) {
        nexts.insert(to.to_string());
    }
}

fn increment_plan_edge_indegree(indegree: &mut std::collections::HashMap<String, usize>, to: &str) {
    if let Some(count) = indegree.get_mut(to) {
        *count += 1;
    }
}

fn take_next_ready_plan_node(ready: &mut BTreeSet<String>) -> Option<String> {
    let id = ready.iter().next().cloned()?;
    ready.remove(&id);
    Some(id)
}

fn drain_plan_sorted_successors(
    adj: &std::collections::HashMap<String, BTreeSet<String>>,
    id: &str,
    indegree: &mut std::collections::HashMap<String, usize>,
    ready: &mut BTreeSet<String>,
) {
    if let Some(nexts) = adj.get(id) {
        for next in nexts {
            update_plan_sorted_successor_indegree(indegree, ready, next);
        }
    }
}

fn update_plan_sorted_successor_indegree(
    indegree: &mut std::collections::HashMap<String, usize>,
    ready: &mut BTreeSet<String>,
    next: &str,
) {
    if let Some(count) = indegree.get_mut(next) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            ready.insert(next.to_string());
        }
    }
}

fn extract_output_log_path(out: &str) -> Option<PathBuf> {
    for line in out.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("output_log:") {
            let path = rest.trim();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        if let Some(idx) = trimmed.find("output_log=") {
            let rest = trimmed[idx + "output_log=".len()..].trim();
            let path = rest.split_whitespace().next().unwrap_or("");
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

fn parse_cargo_test_failures(out: &str) -> Value {
    let mut locations = BTreeSet::new();
    let mut failed_tests = BTreeSet::new();
    let mut stalled_tests = BTreeSet::new();
    let mut failure_block: Vec<String> = Vec::new();
    let mut rerun_hint: Option<String> = None;

    let (log_path, scan) = load_cargo_test_failure_scan(out);

    for line in scan.lines() {
        let trimmed = line.trim();
        parse_cargo_test_failure_line(
            trimmed,
            &mut locations,
            &mut failed_tests,
            &mut stalled_tests,
            &mut failure_block,
            &mut rerun_hint,
        );
    }

    let mut payload = serde_json::Map::new();
    payload.insert(
        "error_locations".to_string(),
        Value::Array(locations.into_iter().map(Value::String).collect()),
    );
    if let Some(path) = log_path {
        payload.insert(
            "output_log".to_string(),
            Value::String(path.display().to_string()),
        );
    }
    if !stalled_tests.is_empty() {
        payload.insert(
            "stalled_tests".to_string(),
            Value::Array(stalled_tests.iter().cloned().map(Value::String).collect()),
        );
        if failed_tests.is_empty() {
            failed_tests.extend(stalled_tests.iter().cloned());
        }
        if rerun_hint.is_none() {
            rerun_hint =
                Some("tests appear stalled; re-run with timeout or inspect output_log".to_string());
        }
    }
    if !failed_tests.is_empty() {
        payload.insert(
            "failed_tests".to_string(),
            Value::Array(failed_tests.into_iter().map(Value::String).collect()),
        );
    }
    if let Some(hint) = rerun_hint {
        payload.insert("rerun_hint".to_string(), Value::String(hint));
    }
    if !failure_block.is_empty() {
        payload.insert(
            "failure_block".to_string(),
            Value::Array(failure_block.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(payload)
}

fn parse_cargo_test_failure_line(
    trimmed: &str,
    locations: &mut BTreeSet<String>,
    failed_tests: &mut BTreeSet<String>,
    stalled_tests: &mut BTreeSet<String>,
    failure_block: &mut Vec<String>,
    rerun_hint: &mut Option<String>,
) {
    if let Some(idx) = trimmed.find(".rs:") {
        let path = &trimmed[..idx + 3];
        let rest = &trimmed[idx + 3..];
        let mut it = rest.splitn(3, ':');
        let line_no = it.next().unwrap_or("");
        let col_no = it.next().unwrap_or("");
        if !line_no.is_empty() && !col_no.is_empty() {
            locations.insert(format!("{}:{}:{}", path, line_no, col_no));
        }
    }
    if let Some(stripped) = trimmed.strip_prefix("test ") {
        if let Some(name) = stripped.strip_suffix(" ... FAILED") {
            failed_tests.insert(name.trim().to_string());
        }
        collect_stalled_test_name(stalled_tests, stripped);
    }
    if rerun_hint.is_none() && trimmed.contains("To rerun") {
        *rerun_hint = Some(trimmed.to_string());
    }
    if trimmed.contains("panicked at")
        || trimmed.contains("FAILED")
        || trimmed.contains("has been running for over")
    {
        failure_block.push(trimmed.to_string());
    }
}

fn load_cargo_test_failure_scan(out: &str) -> (Option<PathBuf>, String) {
    let log_path = extract_output_log_path(out);
    let mut scan = out.to_string();
    if let Some(path) = log_path.as_ref() {
        if let Ok(content) = fs::read_to_string(path) {
            scan = truncate(&content, MAX_SNIPPET * 4).to_string();
        }
    }
    (log_path, scan)
}

fn collect_stalled_test_name(stalled_tests: &mut BTreeSet<String>, stripped: &str) {
    if let Some(name) = [
        " has been running for over 60 seconds",
        " has been running for over 30 seconds",
        " has been running for over 10 seconds",
    ]
    .into_iter()
    .find_map(|suffix| stripped.strip_suffix(suffix))
    {
        stalled_tests.insert(name.trim().to_string());
    }
}

fn load_graph_symbols(
    graph_json: &Path,
) -> Result<std::collections::HashMap<u32, (String, String)>> {
    let content = ctx_read(graph_json)?;
    let value: Value = serde_json::from_str(&content)?;
    let mut out = std::collections::HashMap::new();
    let nodes = value
        .get("nodes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for node in nodes {
        let id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if id == 0 {
            continue;
        }
        let kind = node
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let symbol = node
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.insert(id, (kind, symbol));
    }
    Ok(out)
}

fn load_nodes_symbols(
    nodes_csv: &Path,
) -> Result<std::collections::HashMap<u32, (String, String)>> {
    let content = ctx_read(nodes_csv)?;
    let mut out = std::collections::HashMap::new();
    for (idx, line) in content.lines().enumerate() {
        if idx == 0 {
            continue;
        }
        let mut parts = line.splitn(4, ',');
        let id_raw = parts.next().unwrap_or("").trim();
        let kind = parts.next().unwrap_or("").trim().to_string();
        let symbol = parts.next().unwrap_or("").trim().to_string();
        if let Ok(id) = id_raw.parse::<u32>() {
            if id == 0 {
                continue;
            }
            out.insert(id, (kind, symbol));
        }
    }
    Ok(out)
}

fn symbol_label(map: &std::collections::HashMap<u32, (String, String)>, raw: &str) -> String {
    let id = raw.parse::<u32>().ok();
    if let Some(id) = id {
        if let Some((kind, symbol)) = map.get(&id) {
            if symbol.is_empty() {
                return format!("{id} {kind}").trim().to_string();
            }
            return format!("{id} {kind} {symbol}").trim().to_string();
        }
        return id.to_string();
    }
    raw.to_string()
}

pub(crate) fn patch_scope_error_with_mode(
    role: &str,
    patch: &str,
    self_mod: bool,
) -> Option<String> {
    let raw_targets = patch_targets(patch);
    let normalized_targets: Vec<String> = raw_targets
        .iter()
        .map(|target| normalize_patch_target_for_scope(target))
        .collect();
    let targets: Vec<&str> = normalized_targets.iter().map(|s| s.as_str()).collect();
    if targets.is_empty() {
        return None;
    }

    let touches = classify_patch_targets(&targets);
    match role {
        "solo" => None,
        role if role.starts_with("executor") => executor_patch_scope_error(&touches, self_mod),
        "verifier" | "verifier_a" | "verifier_b" => verifier_patch_scope_error(&touches),
        "planner" | "mini_planner" => planner_patch_scope_error(&targets, &touches),
        "diagnostics" => diagnostics_patch_scope_error(&touches),
        _ => None,
    }
}

fn normalize_patch_target_for_scope(target: &str) -> String {
    let trimmed = target.trim();
    let target_path = Path::new(trimmed);
    if target_path.is_absolute() {
        let ws = Path::new(crate::constants::workspace());
        if let Ok(relative) = target_path.strip_prefix(ws) {
            return relative.to_string_lossy().replace('\\', "/");
        }
    }
    trimmed.strip_prefix("./").unwrap_or(trimmed).to_string()
}

struct PatchTargetTouches {
    diagnostics_file: String,
    legacy_diagnostics_file: &'static str,
    touches_spec: bool,
    touches_lane: bool,
    touches_master_plan: bool,
    touches_violations: bool,
    touches_objectives: bool,
    touches_diagnostics: bool,
    touches_src: bool,
    touches_tests: bool,
    touches_other: bool,
}

fn classify_patch_targets(targets: &[&str]) -> PatchTargetTouches {
    let diagnostics_file = diagnostics_file().to_string();
    let legacy_diagnostics_file = "DIAGNOSTICS.json";
    let touches_spec = targets.iter().any(|path| *path == SPEC_FILE);
    let touches_lane = targets.iter().any(|path| is_lane_plan(path));
    let touches_master_plan = targets.iter().any(|path| *path == MASTER_PLAN_FILE);
    let touches_violations = targets.iter().any(|path| *path == VIOLATIONS_FILE);
    let touches_objectives = targets.iter().any(|path| *path == OBJECTIVES_FILE);
    let touches_diagnostics = targets
        .iter()
        .any(|path| *path == diagnostics_file || *path == legacy_diagnostics_file);
    let touches_src = targets.iter().any(|path| is_src_path(path));
    let touches_tests = targets.iter().any(|path| is_tests_path(path));
    let touches_other = targets.iter().any(|path| {
        *path != SPEC_FILE
            && *path != MASTER_PLAN_FILE
            && !is_src_path(path)
            && !is_tests_path(path)
            && !is_lane_plan(path)
            && *path != VIOLATIONS_FILE
            && *path != OBJECTIVES_FILE
            && *path != diagnostics_file
            && *path != legacy_diagnostics_file
    });
    PatchTargetTouches {
        diagnostics_file,
        legacy_diagnostics_file,
        touches_spec,
        touches_lane,
        touches_master_plan,
        touches_violations,
        touches_objectives,
        touches_diagnostics,
        touches_src,
        touches_tests,
        touches_other,
    }
}

fn executor_patch_scope_error(touches: &PatchTargetTouches, self_mod: bool) -> Option<String> {
    let spec_blocked = touches.touches_spec && !self_mod;
    let tests_blocked = touches.touches_tests && !self_mod;
    if spec_blocked
        || touches.touches_master_plan
        || touches.touches_lane
        || touches.touches_violations
        || touches.touches_diagnostics
        || tests_blocked
        || touches.touches_other
    {
        Some(
            "Executor may not patch plan files, violations, diagnostics, invariants, objectives, tests outside self-modification mode, or out-of-scope files. Execute code/tests only and report evidence in `message.payload`."
                .to_string(),
        )
    } else {
        None
    }
}

fn verifier_patch_scope_error(touches: &PatchTargetTouches) -> Option<String> {
    if touches.touches_violations {
        return Some(
            "apply_patch is ALWAYS rejected for VIOLATIONS.json. \
             Use the `violation` action instead — e.g. \
             {\"action\":\"violation\",\"op\":\"upsert\",\"violation\":{...}} to add/update or \
             {\"action\":\"violation\",\"op\":\"resolve\",\"violation_id\":\"<id>\"} to remove. \
             Retrying apply_patch on VIOLATIONS.json will produce this same error every time."
                .to_string(),
        );
    }
    if touches.touches_spec
        || touches.touches_lane
        || touches.touches_diagnostics
        || touches.touches_other
    {
        Some(
            "Verifier may not patch source files, SPEC.md, lane plans, or diagnostics. Use the `plan` action for PLAN.json updates and the `violation` action for VIOLATIONS.json."
                .to_string(),
        )
    } else {
        Some(
            "Verifier may not use apply_patch here. Use the `violation` action for VIOLATIONS.json and the `plan` action for PLAN.json."
                .to_string(),
        )
    }
}

fn planner_patch_scope_error(targets: &[&str], touches: &PatchTargetTouches) -> Option<String> {
    if touches.touches_master_plan {
        return Some(
            "apply_patch is ALWAYS rejected for PLAN.json. \
             Use the `plan` action instead — e.g. \
             {\"action\":\"plan\",\"op\":\"set_task_status\",\"task_id\":\"<id>\",\"status\":\"ready\",\
             \"rationale\":\"<why>\",\"predicted_next_actions\":[...]}. \
             Retrying apply_patch on PLAN.json will produce this same error every time."
                .to_string(),
        );
    }
    if touches.touches_spec
        || touches.touches_violations
        || targets
            .iter()
            .any(|path| is_src_path(path) || is_tests_path(path))
    {
        Some(
            "apply_patch on `src/`, `tests/`, `SPEC.md`, or `VIOLATIONS.json` is rejected for the planner role. \
             Planner does not write source code. \
             Use the `plan` action to create or update a task in PLAN.json and mark it `ready` so the executor picks it up. \
             Example: {\"action\":\"plan\",\"op\":\"create_task\",\"task\":{\"id\":\"<id>\",\"title\":\"<title>\",\
             \"status\":\"ready\",\"steps\":[\"...\"]},\"rationale\":\"<why>\",\"predicted_next_actions\":[...]}."
                .to_string(),
        )
    } else if touches.touches_lane || touches.touches_objectives {
        None
    } else {
        Some(
            "Planner may patch lane plans or `agent_state/OBJECTIVES.json` only. \
             Use the `plan` action for `PLAN.json` updates; no other patches are allowed."
                .to_string(),
        )
    }
}

fn diagnostics_patch_scope_error(touches: &PatchTargetTouches) -> Option<String> {
    if touches.touches_spec
        || touches.touches_master_plan
        || touches.touches_lane
        || touches.touches_violations
        || touches.touches_src
        || touches.touches_tests
        || touches.touches_other
    {
        Some(format!(
            "Diagnostics may only patch {} or {} because diagnostics owns ranked failure reporting.",
            touches.diagnostics_file,
            touches.legacy_diagnostics_file
        ))
    } else {
        None
    }
}

pub(crate) fn patch_scope_error(role: &str, patch: &str) -> Option<String> {
    patch_scope_error_with_mode(role, patch, is_self_modification_mode())
}

/// Walk up from `file_path` (workspace-relative) to find the nearest Cargo.toml.
/// Returns the package name from that manifest, or None if not found.
fn infer_crate_for_patch(workspace: &Path, file_path: &str) -> Option<String> {
    let mut dir = workspace.join(file_path);
    dir.pop(); // start from parent of the file
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            let text = std::fs::read_to_string(&manifest).ok()?;
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("name") {
                    let name = rest.trim().trim_start_matches('=').trim().trim_matches('"');
                    if !name.is_empty() {
                        return Some(name.to_string());
                    }
                }
            }
        }
        if dir == workspace {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

// ── Patch-anchor auto-read (mirrors harness_repair logic) ─────────────────────

const AUTO_READ_CONTEXT_BEFORE: usize = 20;
const AUTO_READ_CONTEXT_AFTER: usize = 40;

/// Extract the file path from an apply_patch anchor-miss error.
/// Matches: "Failed to find expected lines in PATH:\n..."
fn extract_anchor_fail_path(err_msg: &str) -> Option<String> {
    let prefix = "Failed to find expected lines in ";
    for line in err_msg.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            let path = rest.trim_end_matches(':').trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Parse the indented anchor lines out of the patch error message.
fn extract_expected_anchor_lines(err_msg: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut capture = false;
    for line in err_msg.lines() {
        if line.starts_with("Failed to find expected lines in ") {
            capture = true;
            continue;
        }
        if !capture {
            continue;
        }
        if line.trim().is_empty() {
            if !lines.is_empty() {
                break;
            }
            continue;
        }
        if line.starts_with("    ") || line.starts_with('\t') {
            lines.push(line.trim().to_string());
            continue;
        }
        if !lines.is_empty() {
            break;
        }
    }
    lines
}

fn patch_failure_guidance(path: Option<&str>, err_msg: &str) -> String {
    let mut hints = Vec::new();
    hints.push(
        "Patch anchor miss: deleted/context lines must match the current file EXACTLY.".to_string(),
    );
    hints.push("Do not abbreviate deleted lines like `-1. Centralize d`; copy exact text from read_file output.".to_string());
    hints.push("Next step: emit `read_file` for the target file, then build a new patch with at least 3 unchanged context lines.".to_string());

    if let Some(file) = path {
        let diagnostics_file = diagnostics_file();
        let legacy_diagnostics_file = "DIAGNOSTICS.json";
        if file == diagnostics_file || file == legacy_diagnostics_file || file.ends_with(".md") {
            hints.push("This is a prose/markdown file: prefer rewriting the whole section or the whole file instead of a tiny surgical hunk.".to_string());
            hints.push(format!(
                "For {}, one full-file rewrite is usually more reliable than repeated partial patches.",
                diagnostics_file
            ));
        }
    }

    let anchors = extract_expected_anchor_lines(err_msg);
    if !anchors.is_empty() {
        hints.push(format!("Failed anchor lines: {}", anchors.join(" | ")));
    }

    hints.join("\n")
}

/// Find the file region closest to the failed anchor and return a numbered excerpt.
fn extract_anchor_context_excerpt(full: &str, err_msg: &str) -> Option<(usize, usize, String)> {
    let anchor_lines = extract_expected_anchor_lines(err_msg);
    if anchor_lines.is_empty() {
        return None;
    }
    let file_lines: Vec<&str> = full.lines().collect();
    let mut best_idx: Option<usize> = None;
    for anchor in anchor_lines.iter().rev() {
        let needle = anchor.trim();
        if needle.len() < 8 {
            continue;
        }
        if let Some(idx) = file_lines.iter().position(|l| l.contains(needle)) {
            best_idx = Some(idx);
            break;
        }
    }
    let idx = best_idx?;
    let start_idx = idx.saturating_sub(AUTO_READ_CONTEXT_BEFORE);
    let end_idx = (idx + AUTO_READ_CONTEXT_AFTER + 1).min(file_lines.len());
    let start_line = start_idx + 1;
    let excerpt = file_lines[start_idx..end_idx]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}: {}", start_line + i, l))
        .collect::<Vec<_>>()
        .join("\n");
    Some((start_line, end_idx, excerpt))
}

/// Auto-read the region near the failed anchor, falling back to the full file.
fn auto_read_for_patch_anchor(workspace: &Path, relative: &str, err_msg: &str) -> Result<String> {
    let path = safe_join(workspace, relative)?;
    let full = std::fs::read_to_string(&path)
        .with_context(|| format!("auto-read failed: {}", path.display()))?;
    if let Some((start, end, excerpt)) = extract_anchor_context_excerpt(&full, err_msg) {
        return Ok(format!("Current content near likely match of failed anchor in {relative} (lines {start}-{end}):\n{excerpt}"));
    }
    // Fallback: first MAX_FULL_READ_LINES lines of the file.
    let text = full
        .lines()
        .take(MAX_FULL_READ_LINES)
        .enumerate()
        .map(|(i, l)| format!("{}: {}", i + 1, l))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("Current content of {relative}:\n{text}"))
}

// ── Action executors ───────────────────────────────────────────────────────────

fn exec_list_dir(workspace: &Path, relative: &str) -> Result<String> {
    let path = safe_join(workspace, relative)?;
    let mut entries = std::fs::read_dir(&path)
        .with_context(|| format!("list_dir: {}", path.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    entries.sort();
    Ok(entries.join("\n"))
}

fn exec_read_file(
    workspace: &Path,
    relative: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String> {
    let path = safe_join(workspace, relative)?;
    let full =
        std::fs::read_to_string(&path).with_context(|| format!("read_file: {}", path.display()))?;
    let lines: Vec<&str> = full.lines().collect();
    let total = lines.len();
    let from = start_line.unwrap_or(1).saturating_sub(1).min(total);
    let max_lines = match (start_line, end_line) {
        (Some(start), Some(end)) if end >= start && end > 0 => {
            let end_idx = end.saturating_sub(1).min(total.saturating_sub(1));
            let start_idx = start.saturating_sub(1).min(total);
            end_idx.saturating_sub(start_idx).saturating_add(1)
        }
        (Some(_), Some(_)) => 0,
        (Some(_), None) => 500,
        (None, _) => MAX_FULL_READ_LINES,
    };
    let text = lines[from..]
        .iter()
        .take(max_lines)
        .enumerate()
        .map(|(i, l)| format!("{}: {}", from + i + 1, l))
        .collect::<Vec<_>>()
        .join("\n");
    let shown = max_lines.min(total.saturating_sub(from));
    if total > from + shown {
        Ok(format!(
            "{text}\n(file has {total} lines total; use \"line\":{} or \"line_end\" to read more)",
            from + shown + 1
        ))
    } else {
        Ok(text)
    }
}

fn handle_message_action(
    role: &str,
    step: usize,
    action: &Value,
    mut writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let status = action.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let payload = action
        .get("payload")
        .cloned()
        .unwrap_or_else(|| Value::Null);
    let summary = payload
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("message accepted");
    let full_message = serde_json::to_string_pretty(action).unwrap_or_else(|_| "{}".to_string());
    let msg_type = action.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let to_role = action.get("to").and_then(|v| v.as_str()).unwrap_or("");
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let _ = std::fs::create_dir_all(agent_state_dir);
    let normalized_role = role
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let normalized_to = to_role
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");

    if normalized_role == normalized_to
        && !is_allowed_self_addressed_message(action, &normalized_role, &normalized_to)
    {
        return Ok((
            true,
            format!(
                "invalid self-addressed message suppressed: {normalized_role} -> {normalized_to}"
            ),
        ));
    }

    if let Some(result) = suppress_redundant_planner_blocker(
        role,
        msg_type,
        &payload,
        agent_state_dir,
        writer.as_deref_mut(),
    )
    {
        return Ok(result);
    }

    sync_verifier_blocker_state(
        role,
        to_role,
        msg_type,
        status,
        summary,
        &payload,
        agent_state_dir,
        writer.as_deref_mut(),
    );
    persist_inbound_message(
        role,
        step,
        std::path::Path::new(crate::constants::workspace()),
        action,
        &full_message,
        writer.as_deref_mut(),
    );

    // Capture blocker messages as first-class artifact for invariant synthesis.
    if msg_type == "blocker" && status == "blocked" {
        let task_id = action.get("task_id").and_then(|v| v.as_str());
        let objective_id = action
            .get("objective_id")
            .or_else(|| payload.get("objective_id"))
            .and_then(|v| v.as_str());
        crate::blockers::record_blocker_message_with_writer(
            std::path::Path::new(crate::constants::workspace()),
            writer.as_deref_mut(),
            role,
            summary,
            task_id,
            objective_id,
        );
    }

    Ok((
        true,
        format!("{summary}\n\nmessage_action:\n{full_message}"),
    ))
}

fn suppress_redundant_planner_blocker(
    role: &str,
    msg_type: &str,
    payload: &Value,
    agent_state_dir: &Path,
    mut writer: Option<&mut CanonicalWriter>,
) -> Option<(bool, String)> {
    if role != "planner" || msg_type != "blocker" {
        return None;
    }
    let evidence = payload
        .get("evidence")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if evidence.is_empty() {
        return None;
    }
    let evidence_hash = artifact_write_signature(&["planner_blocker_evidence", &evidence]);
    if let Some(w) = writer.as_deref_mut() {
        if w.state().planner_blocker_evidence_hash == evidence_hash {
            return Some((
                false,
                "Error executing action: planner blocker suppressed: evidence unchanged. \
                 Retry with materially updated evidence or choose a different action."
                    .to_string(),
            ));
        }
        w.apply(ControlEvent::PlannerBlockerEvidenceSet {
            evidence_hash: evidence_hash.clone(),
        });
    } else {
        let evidence_path = agent_state_dir.join("last_planner_blocker_evidence.txt");
        if let Ok(prev) = std::fs::read_to_string(&evidence_path) {
            if prev.trim() == evidence {
                return Some((
                    false,
                    "Error executing action: planner blocker suppressed: evidence unchanged. \
                     Retry with materially updated evidence or choose a different action."
                        .to_string(),
                ));
            }
        }
    }
    let evidence_path = agent_state_dir.join("last_planner_blocker_evidence.txt");
    let _ = std::fs::write(evidence_path, evidence);
    None
}

fn is_allowed_self_addressed_message(action: &Value, from_role: &str, to_role: &str) -> bool {
    from_role == "solo"
        && to_role == "solo"
        && action.get("action").and_then(|v| v.as_str()) == Some("message")
        && action
            .get("type")
            .and_then(|v| v.as_str())
            .is_some_and(|kind| kind.eq_ignore_ascii_case("result"))
        && action
            .get("status")
            .and_then(|v| v.as_str())
            .is_some_and(|status| status.eq_ignore_ascii_case("complete"))
}

fn sync_verifier_blocker_state(
    role: &str,
    to_role: &str,
    msg_type: &str,
    status: &str,
    summary: &str,
    payload: &Value,
    agent_state_dir: &Path,
    mut writer: Option<&mut CanonicalWriter>,
) {
    if !to_role.eq_ignore_ascii_case("verifier") {
        return;
    }
    let active_path = agent_state_dir.join("active_blocker_to_verifier.json");
    if msg_type == "blocker" && status == "blocked" {
        if let Some(writer) = writer.as_deref_mut() {
            writer.apply(ControlEvent::VerifierBlockerSet { active: true });
        }
        let blocker_state = json!({
            "from": role,
            "summary": summary,
            "evidence": payload.get("evidence").and_then(|v| v.as_str()).unwrap_or(""),
            "required_action": payload.get("required_action").and_then(|v| v.as_str()).unwrap_or(""),
            "severity": payload.get("severity").and_then(|v| v.as_str()).unwrap_or(""),
        });
        let _ = std::fs::write(
            &active_path,
            serde_json::to_string_pretty(&blocker_state).unwrap_or_default(),
        );
    } else {
        if let Some(writer) = writer.as_deref_mut() {
            writer.apply(ControlEvent::VerifierBlockerSet { active: false });
        }
    }
    if active_path.exists() && !(msg_type == "blocker" && status == "blocked") {
        let _ = std::fs::remove_file(active_path);
    }
}

fn handle_list_dir_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let path = action
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("list_dir missing 'path'"))?;
    let out = exec_list_dir(workspace, path)?;
    Ok((false, format!("list_dir {path}:\n{out}")))
}

fn handle_read_file_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let path = action
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("read_file missing 'path'"))?;
    let line = action
        .get("line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let line_start = action
        .get("line_start")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let line_end = action
        .get("line_end")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let start = line_start.or(line);
    let out = exec_read_file(workspace, path, start, line_end)?;
    let receipt_id = append_evidence_receipt(
        role,
        step,
        "read_file",
        Some(path),
        Some(workspace.join(path)),
        json!({"line": line, "line_start": line_start, "line_end": line_end}),
        &out,
    )
    .ok();
    eprintln!(
        "[{role}] step={} read_file path={path} bytes={}",
        step,
        out.len()
    );
    Ok((
        false,
        format_output_with_evidence_receipt(
            &format!("read_file {path}"),
            &out,
            receipt_id.as_deref(),
        ),
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SymbolSpan {
    start: usize,
    end: usize,
    line: usize,
    column: usize,
    end_line: usize,
    end_column: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SymbolEntry {
    name: String,
    kind: String,
    file: String,
    span: SymbolSpan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SymbolsIndexFile {
    version: u32,
    symbols: Vec<SymbolEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RenameCandidate {
    name: String,
    kind: String,
    file: String,
    span: SymbolSpan,
    score: u32,
    reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RenameCandidatesFile {
    version: u32,
    source_symbols_path: String,
    candidates: Vec<RenameCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PreparedRenameActionFile {
    version: u32,
    source_candidates_path: String,
    selected_index: usize,
    selected_candidate: RenameCandidate,
    rename_action: Value,
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn offset_to_line_col(text: &str, starts: &[usize], offset: usize) -> (usize, usize) {
    let idx = match starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    let line_start = starts[idx];
    let line = idx + 1;
    let col = text[line_start..offset].chars().count() + 1;
    (line, col)
}

fn symbol_kind_from_name_owner(owner_kind: SyntaxKind) -> Option<&'static str> {
    match owner_kind {
        SyntaxKind::FN => Some("function"),
        SyntaxKind::STRUCT => Some("struct"),
        SyntaxKind::ENUM => Some("enum"),
        SyntaxKind::TRAIT => Some("trait"),
        SyntaxKind::TYPE_ALIAS => Some("type_alias"),
        SyntaxKind::CONST => Some("const"),
        SyntaxKind::STATIC => Some("static"),
        SyntaxKind::MODULE => Some("module"),
        SyntaxKind::UNION => Some("union"),
        SyntaxKind::VARIANT => Some("enum_variant"),
        SyntaxKind::RECORD_FIELD => Some("field"),
        SyntaxKind::TYPE_PARAM => Some("type_param"),
        _ => None,
    }
}

fn collect_rust_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if collect_rust_file_root(root, out) {
        return Ok(());
    }
    let entries = sorted_dir_entries(root)?;
    for entry in entries {
        collect_rust_dir_entry(entry, out)?;
    }
    Ok(())
}

fn collect_rust_file_root(root: &Path, out: &mut Vec<PathBuf>) -> bool {
    if !root.is_file() {
        return false;
    }
    if is_rust_file(root) {
        out.push(root.to_path_buf());
    }
    true
}

fn sorted_dir_entries(root: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("read_dir {}", root.display()))? {
        entries.push(entry?);
    }
    entries.sort_by(|a, b| a.path().cmp(&b.path()));
    Ok(entries)
}

fn collect_rust_dir_entry(entry: fs::DirEntry, out: &mut Vec<PathBuf>) -> Result<()> {
    let path = entry.path();
    let file_type = entry.file_type()?;
    if file_type.is_dir() {
        if is_ignored_dir_entry(&entry) {
            return Ok(());
        }
        collect_rust_files(&path, out)?;
        return Ok(());
    }
    if file_type.is_file() && is_rust_file(&path) {
        out.push(path);
    }
    Ok(())
}

fn is_ignored_dir_entry(entry: &fs::DirEntry) -> bool {
    let name = entry.file_name();
    let name = name.to_string_lossy();
    is_ignored_dir(name.as_ref())
}

fn is_rust_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("rs")
}

fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | ".idea" | ".vscode"
    )
}

fn extract_decl_symbols(workspace: &Path, file_path: &Path, text: &str) -> Vec<SymbolEntry> {
    let parse = SourceFile::parse(text, Edition::CURRENT);
    if !parse.errors().is_empty() {
        return Vec::new();
    }
    let root = parse.tree();
    let starts = line_starts(text);
    let file_rel = file_path
        .strip_prefix(workspace)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| file_path.to_string_lossy().replace('\\', "/"));
    let mut out = Vec::new();
    for token in root
        .syntax()
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
    {
        if let Some(entry) = symbol_entry_from_token(text, &starts, &file_rel, token) {
            out.push(entry);
        }
    }
    out
}

fn symbol_entry_from_token(
    text: &str,
    starts: &[usize],
    file_rel: &str,
    token: SyntaxToken,
) -> Option<SymbolEntry> {
    if token.kind() != SyntaxKind::IDENT {
        return None;
    }
    let name_node = token.parent()?;
    if name_node.kind() != SyntaxKind::NAME {
        return None;
    }
    let owner = name_node.parent()?;
    let kind = symbol_kind_from_name_owner(owner.kind())?;
    let range = token.text_range();
    let start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;
    let (line, column) = offset_to_line_col(text, starts, start);
    let (end_line, end_column) = offset_to_line_col(text, starts, end);
    Some(SymbolEntry {
        name: token.text().to_string(),
        kind: kind.to_string(),
        file: file_rel.to_string(),
        span: SymbolSpan {
            start,
            end,
            line,
            column,
            end_line,
            end_column,
        },
    })
}

fn handle_symbols_index_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let path_raw = action.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let out_raw = action
        .get("out")
        .and_then(|v| v.as_str())
        .unwrap_or("state/symbols.json");
    let scan_root = safe_join(workspace, path_raw)?;
    let out_path = safe_join(workspace, out_raw)?;
    let payload = build_symbols_index_payload(workspace, &scan_root)?;
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create symbols output dir {}", parent.display()))?;
    }
    fs::write(
        &out_path,
        serde_json::to_string_pretty(&payload).context("serialize symbols index")?,
    )
    .with_context(|| format!("write {}", out_path.display()))?;
    Ok((
        false,
        format!(
            "symbols_index ok: output={} symbols={}",
            out_raw,
            payload.symbols.len()
        ),
    ))
}

fn build_symbols_index_payload(workspace: &Path, scan_root: &Path) -> Result<SymbolsIndexFile> {
    let mut files = Vec::new();
    collect_rust_files(scan_root, &mut files)?;
    files.sort();

    let mut symbols = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap_or_default();
        symbols.extend(extract_decl_symbols(workspace, &file, &text));
    }
    symbols.sort_by(|a, b| {
        (
            a.file.as_str(),
            a.span.start,
            a.span.end,
            a.kind.as_str(),
            a.name.as_str(),
        )
            .cmp(&(
                b.file.as_str(),
                b.span.start,
                b.span.end,
                b.kind.as_str(),
                b.name.as_str(),
            ))
    });
    symbols.dedup_by(|a, b| {
        a.file == b.file
            && a.span.start == b.span.start
            && a.span.end == b.span.end
            && a.kind == b.kind
            && a.name == b.name
    });

    Ok(SymbolsIndexFile {
        version: 1,
        symbols,
    })
}

fn ambiguous_name_reasons(name: &str) -> Vec<String> {
    let lower = name.to_ascii_lowercase();
    let vague = [
        "tmp", "temp", "data", "info", "item", "obj", "val", "foo", "bar", "baz", "util", "helper",
        "thing", "stuff", "misc",
    ];
    let mut reasons = Vec::new();
    if lower.len() <= 2 {
        reasons.push("name is very short".to_string());
    }
    if vague.contains(&lower.as_str()) {
        reasons.push("name is ambiguous/generic".to_string());
    }
    if lower.ends_with('_') || lower.contains("__") {
        reasons.push("name shape suggests low clarity".to_string());
    }
    reasons
}

fn split_prefix_and_stem(name: &str) -> Option<(&'static str, String)> {
    let prefixes: [(&str, &str); 12] = [
        ("get_", "get"),
        ("fetch_", "fetch"),
        ("load_", "load"),
        ("read_", "read"),
        ("build_", "build"),
        ("make_", "make"),
        ("create_", "create"),
        ("set_", "set"),
        ("update_", "update"),
        ("handle_", "handle"),
        ("process_", "process"),
        ("compute_", "compute"),
    ];
    prefixes.into_iter().find_map(|(needle, tag)| {
        name.strip_prefix(needle)
            .filter(|rest| !rest.is_empty())
            .map(|rest| (tag, rest.to_string()))
    })
}

fn handle_symbols_rename_candidates_action(
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let (symbols_path_raw, out_raw, symbols_path, out_path) =
        parse_symbols_rename_candidates_paths(workspace, action)?;
    let symbols_file = load_symbols_index_file(&symbols_path)?;
    let prefixes_by_stem = build_function_prefixes_by_stem(&symbols_file);

    let identity_surface_names = rename_candidate_identity_surface_names();
    let identity_surface_files = rename_candidate_identity_surface_files();
    let candidates = collect_rename_candidates(
        &symbols_file,
        &prefixes_by_stem,
        &identity_surface_names,
        &identity_surface_files,
    );

    finalize_rename_candidates_output(candidates, &symbols_path_raw, &out_raw, &out_path)
}

fn parse_symbols_rename_candidates_paths(
    workspace: &Path,
    action: &Value,
) -> Result<(String, String, PathBuf, PathBuf)> {
    let symbols_path_raw = action
        .get("symbols_path")
        .and_then(|v| v.as_str())
        .unwrap_or("state/symbols.json")
        .to_string();
    let out_raw = action
        .get("out")
        .and_then(|v| v.as_str())
        .unwrap_or("state/rename_candidates.json")
        .to_string();
    let symbols_path = safe_join(workspace, &symbols_path_raw)?;
    let out_path = safe_join(workspace, &out_raw)?;
    Ok((symbols_path_raw, out_raw, symbols_path, out_path))
}

fn load_symbols_index_file(symbols_path: &Path) -> Result<SymbolsIndexFile> {
    let symbols_text = fs::read_to_string(symbols_path)
        .with_context(|| format!("read {}", symbols_path.display()))?;
    serde_json::from_str(&symbols_text).context("parse symbols index json")
}

fn build_function_prefixes_by_stem(
    symbols_file: &SymbolsIndexFile,
) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    let mut prefixes_by_stem: std::collections::BTreeMap<
        String,
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    for sym in &symbols_file.symbols {
        if sym.kind == "function" {
            if let Some((prefix, stem)) = split_prefix_and_stem(&sym.name) {
                prefixes_by_stem
                    .entry(stem)
                    .or_default()
                    .insert(prefix.to_string());
            }
        }
    }
    prefixes_by_stem
}

fn rename_candidate_identity_surface_names() -> BTreeSet<&'static str> {
    ["id", "endpoint_id", "lane_id"].into_iter().collect()
}

fn rename_candidate_identity_surface_files() -> BTreeSet<&'static str> {
    [
        "src/constants.rs",
        "src/protocol.rs",
        "src/app.rs",
        "src/logging.rs",
    ]
    .into_iter()
    .collect()
}

fn collect_rename_candidates(
    symbols_file: &SymbolsIndexFile,
    prefixes_by_stem: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    identity_surface_names: &BTreeSet<&'static str>,
    identity_surface_files: &BTreeSet<&'static str>,
) -> Vec<RenameCandidate> {
    let mut candidates = Vec::new();
    for sym in &symbols_file.symbols {
        if let Some(candidate) = build_rename_candidate(
            sym,
            prefixes_by_stem,
            identity_surface_names,
            identity_surface_files,
        ) {
            candidates.push(candidate);
        }
    }
    candidates
}

fn build_rename_candidate(
    sym: &SymbolEntry,
    prefixes_by_stem: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    identity_surface_names: &BTreeSet<&'static str>,
    identity_surface_files: &BTreeSet<&'static str>,
) -> Option<RenameCandidate> {
    if should_skip_rename_candidate_symbol(sym, identity_surface_names, identity_surface_files) {
        return None;
    }

    let reasons = rename_candidate_reasons(sym, prefixes_by_stem);
    if reasons.is_empty() {
        return None;
    }

    Some(RenameCandidate {
        name: sym.name.clone(),
        kind: sym.kind.clone(),
        file: sym.file.clone(),
        span: sym.span.clone(),
        score: score_rename_candidate_reasons(&reasons),
        reasons,
    })
}

fn should_skip_rename_candidate_symbol(
    sym: &SymbolEntry,
    identity_surface_names: &BTreeSet<&'static str>,
    identity_surface_files: &BTreeSet<&'static str>,
) -> bool {
    // Field-level symbols are currently not resolvable by the semantic rename tool:
    // `symbol_occurrences` delegates to `resolve_symbol_key`, which only matches graph
    // node keys/suffixes, while the graph does not expose record fields as standalone
    // node identities. Skip them here so prepared rename actions stay executable.
    if sym.kind == "field" {
        return true;
    }

    // Exclude conventional status/result enum variants that are semantically
    // meaningful and should not be mechanically renamed.
    if sym.kind == "enum_variant" && matches!(sym.name.as_str(), "Ok" | "Err" | "Some" | "None") {
        return true;
    }

    // Defense in depth: endpoint/protocol identity names are part of external routing,
    // persistence, and filename surfaces in known authority files. Exclude them even if
    // a future symbol-index/runtime mismatch reclassifies them away from `field`.
    identity_surface_names.contains(sym.name.as_str())
        && identity_surface_files.contains(sym.file.as_str())
}

fn rename_candidate_reasons(
    sym: &SymbolEntry,
    prefixes_by_stem: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) -> Vec<String> {
    let mut reasons = ambiguous_name_reasons(&sym.name);
    if let Some(reason) = inconsistent_function_prefix_reason(sym, prefixes_by_stem) {
        reasons.push(reason);
    }
    reasons
}

fn inconsistent_function_prefix_reason(
    sym: &SymbolEntry,
    prefixes_by_stem: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) -> Option<String> {
    if sym.kind != "function" {
        return None;
    }

    let (prefix, stem) = split_prefix_and_stem(&sym.name)?;
    let prefixes = prefixes_by_stem.get(&stem)?;
    if prefixes.len() <= 1 {
        return None;
    }

    let mut other = other_prefixes(prefixes, prefix);
    other.sort();
    Some(format!(
        "inconsistent verb prefix for stem '{stem}' (also: {})",
        other.join(", ")
    ))
}

fn other_prefixes(prefixes: &std::collections::BTreeSet<String>, prefix: &str) -> Vec<String> {
    prefixes
        .iter()
        .filter(|p| p.as_str() != prefix)
        .cloned()
        .collect()
}

fn finalize_rename_candidates_output(
    mut candidates: Vec<RenameCandidate>,
    symbols_path_raw: &str,
    out_raw: &str,
    out_path: &Path,
) -> Result<(bool, String)> {
    sort_and_dedup_rename_candidates(&mut candidates);
    let payload = RenameCandidatesFile {
        version: 1,
        source_symbols_path: symbols_path_raw.to_string(),
        candidates,
    };
    write_rename_candidates_payload(out_path, &payload)?;
    Ok((false, rename_candidates_success_message(out_raw, &payload)))
}

fn rename_candidates_success_message(out_raw: &str, payload: &RenameCandidatesFile) -> String {
    format!(
        "symbols_rename_candidates ok: output={} candidates={}",
        out_raw,
        payload.candidates.len()
    )
}

fn score_rename_candidate_reasons(reasons: &[String]) -> u32 {
    let mut score = 10u32;
    for reason in reasons {
        if reason.contains("inconsistent verb prefix") {
            score += 30;
        } else if reason.contains("ambiguous/generic") {
            score += 20;
        } else if reason.contains("very short") {
            score += 10;
        } else {
            score += 5;
        }
    }
    score
}

fn sort_and_dedup_rename_candidates(candidates: &mut Vec<RenameCandidate>) {
    candidates.sort_by(|a, b| {
        (
            std::cmp::Reverse(a.score),
            a.file.as_str(),
            a.span.start,
            a.name.as_str(),
            a.kind.as_str(),
        )
            .cmp(&(
                std::cmp::Reverse(b.score),
                b.file.as_str(),
                b.span.start,
                b.name.as_str(),
                b.kind.as_str(),
            ))
    });
    candidates.dedup_by(|a, b| {
        a.file == b.file
            && a.span.start == b.span.start
            && a.span.end == b.span.end
            && a.name == b.name
            && a.kind == b.kind
    });
}

fn write_rename_candidates_payload(out_path: &Path, payload: &RenameCandidatesFile) -> Result<()> {
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create rename candidates output dir {}", parent.display()))?;
    }
    fs::write(
        out_path,
        serde_json::to_string_pretty(payload).context("serialize rename candidates json")?,
    )
    .with_context(|| format!("write {}", out_path.display()))?;
    Ok(())
}

fn handle_symbols_prepare_rename_action(
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let candidates_path_raw = action
        .get("candidates_path")
        .and_then(|v| v.as_str())
        .unwrap_or("state/rename_candidates.json");
    let out_raw = action
        .get("out")
        .and_then(|v| v.as_str())
        .unwrap_or("state/next_rename_action.json");
    let index = action.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let candidates_path = safe_join(workspace, candidates_path_raw)?;
    let out_path = safe_join(workspace, out_raw)?;
    let candidates_text = fs::read_to_string(&candidates_path)
        .with_context(|| format!("read {}", candidates_path.display()))?;
    let candidates_file: RenameCandidatesFile =
        serde_json::from_str(&candidates_text).context("parse rename candidates json")?;
    let selected = selected_rename_candidate(&candidates_file, index, candidates_path_raw)?;
    let rename_action = build_prepared_rename_action(&selected);
    let payload = PreparedRenameActionFile {
        version: 1,
        source_candidates_path: candidates_path_raw.to_string(),
        selected_index: index,
        selected_candidate: selected,
        rename_action,
    };
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create prepared rename output dir {}", parent.display()))?;
    }
    fs::write(
        &out_path,
        serde_json::to_string_pretty(&payload).context("serialize prepared rename action json")?,
    )
    .with_context(|| format!("write {}", out_path.display()))?;
    Ok((
        false,
        format!(
            "symbols_prepare_rename ok: output={} selected_index={} selected_name={}",
            out_raw, payload.selected_index, payload.selected_candidate.name
        ),
    ))
}

fn build_prepared_rename_action(selected: &RenameCandidate) -> Value {
    json!({
        "action": "rename_symbol",
        "old_symbol": selected.name,
        "new_symbol": format!("{}_renamed", selected.name),
        "question": "Does this selected candidate represent the exact symbol to rename across the crate without changing intended behavior?",
        "rationale": "Apply a span-backed rename candidate deterministically and validate impacts immediately after.",
        "predicted_next_actions": [
            {"action": "cargo_test", "intent": "Run focused tests for the touched area after rename."},
            {"action": "run_command", "intent": "Run cargo check to verify workspace compile health."}
        ]
    })
}

fn selected_rename_candidate(
    candidates_file: &RenameCandidatesFile,
    index: usize,
    candidates_path_raw: &str,
) -> Result<RenameCandidate> {
    if candidates_file.candidates.is_empty() {
        bail!(
            "symbols_prepare_rename: no candidates in {}",
            candidates_path_raw
        );
    }
    if index >= candidates_file.candidates.len() {
        bail!(
            "symbols_prepare_rename: index {} out of range (candidates={})",
            index,
            candidates_file.candidates.len()
        );
    }
    Ok(candidates_file.candidates[index].clone())
}

fn handle_rename_symbol_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let idx = load_semantic(workspace, action)?;
    let crate_name = semantic_crate_name(action);
    let pairs = parse_rename_symbol_pairs(action, &crate_name)?;
    let rename_env = capture_rename_symbol_environment(workspace)?;

    let report =
        crate::rename_semantic::rename_symbols_via_semantic_spans(workspace, &idx, &pairs)?;
    eprintln!(
        "[{role}] step={} rename_symbol spans pairs={} replacements={} files={}",
        step,
        pairs.len(),
        report.replacements,
        report.touched_files.len()
    );

    // Post-rename cargo check.  On failure roll back every touched file to
    // its pre-rename state via `git checkout <head> -- <file>...` and
    // surface the compiler output so the agent can diagnose the problem.
    // Skipped when the workspace has no Cargo.toml (e.g. unit-test fixtures).
    run_post_rename_cargo_check(workspace, &rename_env, &report)?;

    Ok((
        false,
        format!(
            "rename_symbol ok: pairs={} replacements={} touched_files={} cargo_check={}",
            pairs.len(),
            report.replacements,
            report.touched_files.len(),
            if rename_env.has_cargo {
                "ok"
            } else {
                "skipped"
            },
        ),
    ))
}

struct RenameSymbolEnvironment {
    in_git: bool,
    has_cargo: bool,
    head: String,
}

fn parse_rename_symbol_pairs(action: &Value, crate_name: &str) -> Result<Vec<(String, String)>> {
    reject_legacy_rename_fields(action)?;

    if let Some(arr) = action.get("renames").and_then(|v| v.as_array()) {
        return parse_bulk_renames(arr, crate_name);
    }

    parse_single_rename(action, crate_name)
}

fn reject_legacy_rename_fields(action: &Value) -> Result<()> {
    if action.get("path").is_some()
        || action.get("line").is_some()
        || action.get("column").is_some()
        || action.get("old_name").is_some()
        || action.get("new_name").is_some()
    {
        bail!("rename_symbol v2 uses `old_symbol`/`new_symbol` (or `renames`) and rustc graph spans; line/column payloads are deprecated");
    }
    Ok(())
}

fn parse_bulk_renames(arr: &[Value], crate_name: &str) -> Result<Vec<(String, String)>> {
    if arr.is_empty() {
        bail!("rename_symbol: `renames` must not be empty");
    }

    let mut pairs = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let old = item
            .get("old")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("rename_symbol: renames[{i}] missing `old`"))?;
        let new = item
            .get("new")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("rename_symbol: renames[{i}] missing `new`"))?;

        let (old, new) = normalize_pair(crate_name, old, new)?;
        pairs.push((old, new));
    }
    Ok(pairs)
}

fn parse_single_rename(action: &Value, crate_name: &str) -> Result<Vec<(String, String)>> {
    let old = action
        .get("old_symbol")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow!("rename_symbol missing non-empty `old_symbol` (or provide `renames`)")
        })?;

    let new = action
        .get("new_symbol")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow!("rename_symbol missing non-empty `new_symbol` (or provide `renames`)")
        })?;

    let (old, new) = normalize_pair(crate_name, old, new)?;
    Ok(vec![(old, new)])
}

fn normalize_pair(crate_name: &str, old: &str, new: &str) -> Result<(String, String)> {
    let old = strip_semantic_crate_prefix(crate_name, old);
    let new = strip_semantic_crate_prefix(crate_name, new);
    if old.is_empty() || new.is_empty() {
        bail!("rename_symbol requires non-empty old/new symbols");
    }
    Ok((old.to_string(), new.to_string()))
}

fn capture_rename_symbol_environment(workspace: &Path) -> Result<RenameSymbolEnvironment> {
    let in_git = workspace.join(".git").exists();
    let has_cargo = workspace.join("Cargo.toml").exists();
    let head = load_git_head(workspace, in_git)?;
    Ok(RenameSymbolEnvironment {
        in_git,
        has_cargo,
        head,
    })
}

fn load_git_head(workspace: &Path, in_git: bool) -> Result<String> {
    if !in_git {
        return Ok(String::new());
    }

    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .context("git rev-parse HEAD")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_post_rename_cargo_check(
    workspace: &Path,
    rename_env: &RenameSymbolEnvironment,
    report: &crate::rename_semantic::RenameReport,
) -> Result<()> {
    if !rename_env.has_cargo {
        return Ok(());
    }

    let check_out = Command::new("cargo")
        .args(["check", "--workspace"])
        .current_dir(workspace)
        .output()
        .context("cargo check --workspace")?;

    if check_out.status.success() {
        return Ok(());
    }

    if rename_env.in_git && !rename_env.head.is_empty() {
        let mut restore_args = vec![
            "checkout".to_string(),
            rename_env.head.clone(),
            "--".to_string(),
        ];
        for f in &report.touched_files {
            restore_args.push(f.to_string_lossy().into_owned());
        }
        let restore_args_ref: Vec<&str> = restore_args.iter().map(String::as_str).collect();
        let _ = Command::new("git")
            .args(&restore_args_ref)
            .current_dir(workspace)
            .output();
    }

    let stderr = String::from_utf8_lossy(&check_out.stderr);
    let stdout = String::from_utf8_lossy(&check_out.stdout);
    let compiler_output = format!("{stdout}{stderr}");
    persist_rename_symbol_errors(workspace, &compiler_output);

    bail!(
        "rename_symbol: cargo check failed after rename — rolled back {} file(s) to {}. Errors written to state/rename_errors.txt.\n{}",
        report.touched_files.len(),
        rename_env.head,
        compiler_output,
    );
}

fn persist_rename_symbol_errors(workspace: &Path, compiler_output: &str) {
    let errors_path = workspace.join("state/rename_errors.txt");
    persist_text_file(&errors_path, compiler_output);
}

fn persist_text_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, content);
}

/// Validate that a just-written JSON state file conforms to its canonical schema.
/// Returns `Some(rejection_message)` if the file violates the schema, `None` if valid
/// or if the file is not a schema-guarded type.
/// Uses `additionalProperties: false` so any extra field is caught and reported to the LLM.
fn validate_state_file_schema(file_path: &str, content: &str) -> Option<String> {
    use crate::reports::{DiagnosticsReport, ViolationsReport};
    use jsonschema::JSONSchema;
    use schemars::schema_for;
    use std::sync::OnceLock;

    let json_value = match serde_json::from_str::<serde_json::Value>(content) {
        Ok(v) => v,
        Err(e) => {
            return Some(format!(
                "apply_patch rejected: file is not valid JSON after patch: {e}"
            ))
        }
    };

    let diag = diagnostics_file();
    let legacy_diag = "DIAGNOSTICS.json";

    if file_path == diag || file_path == legacy_diag {
        static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
        let compiled = SCHEMA.get_or_init(|| {
            let mut val =
                serde_json::to_value(schema_for!(DiagnosticsReport)).expect("diagnostics schema");
            // Enforce no additional properties beyond the canonical four fields.
            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "additionalProperties".to_string(),
                    serde_json::Value::Bool(false),
                );
            }
            JSONSchema::compile(&val).expect("compile diagnostics schema")
        });
        if let Err(errors) = compiled.validate(&json_value) {
            let msgs: Vec<String> = errors.take(5).map(|e| e.to_string()).collect();
            return Some(format!(
                "apply_patch rejected: DiagnosticsReport schema violation\n{}\n\
                 Canonical fields: status, inputs_scanned, ranked_failures, planner_handoff.\n\
                 No additional fields are permitted. Remove any extra fields and retry.",
                msgs.join("\n")
            ));
        }
    } else if file_path == VIOLATIONS_FILE {
        static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
        let compiled = SCHEMA.get_or_init(|| {
            let mut val =
                serde_json::to_value(schema_for!(ViolationsReport)).expect("violations schema");
            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "additionalProperties".to_string(),
                    serde_json::Value::Bool(false),
                );
            }
            JSONSchema::compile(&val).expect("compile violations schema")
        });
        if let Err(errors) = compiled.validate(&json_value) {
            let msgs: Vec<String> = errors.take(5).map(|e| e.to_string()).collect();
            return Some(format!(
                "apply_patch rejected: ViolationsReport schema violation\n{}\n\
                 Canonical fields: status, summary, violations (each with: id, title, severity, \
                 evidence, issue, impact, required_fix, files).\n\
                 No additional fields are permitted. Remove any extra fields and retry.",
                msgs.join("\n")
            ));
        }
    }

    None
}

fn apply_patch_diagnostics_target_path(role: &str, patch: &str) -> Option<String> {
    if role != "diagnostics" {
        return None;
    }
    let current_diagnostics_file = diagnostics_file();
    let legacy_diagnostics_file = "DIAGNOSTICS.json";
    patch_targets(patch).into_iter().find_map(|path| {
        if path == current_diagnostics_file || path == legacy_diagnostics_file {
            Some(path.to_string())
        } else {
            None
        }
    })
}

fn previous_diagnostics_patch_text(
    workspace: &Path,
    diagnostics_target_path: Option<&str>,
) -> Option<String> {
    let diagnostics_target_path = diagnostics_target_path?;
    Some(fs::read_to_string(workspace.join(diagnostics_target_path)).unwrap_or_default())
}

fn schema_guard_snapshots_for_patch(
    workspace: &Path,
    patch: &str,
) -> Vec<(String, Option<String>)> {
    let diag_file = diagnostics_file();
    let legacy_diag = "DIAGNOSTICS.json";
    patch_targets(patch)
        .into_iter()
        .filter(|p| *p == VIOLATIONS_FILE || *p == diag_file || *p == legacy_diag)
        .map(|p| {
            let content = fs::read_to_string(workspace.join(p)).ok();
            (p.to_string(), content)
        })
        .collect()
}

fn reject_unvalidated_diagnostics_persistence(
    role: &str,
    step: usize,
    workspace: &Path,
    diagnostics_targeted: bool,
    diagnostics_target_path: Option<&str>,
    previous_diagnostics_text: Option<String>,
) -> Result<Option<(bool, String)>> {
    if !diagnostics_targeted {
        return Ok(None);
    }
    let diagnostics_path = workspace.join(diagnostics_target_path.unwrap_or(diagnostics_file()));
    let new_diagnostics_text = fs::read_to_string(&diagnostics_path).unwrap_or_default();
    let derived = crate::prompt_inputs::reconcile_diagnostics_report(workspace);
    if new_diagnostics_text == derived {
        return Ok(None);
    }
    if let Some(previous) = previous_diagnostics_text {
        if let Ok(report) = serde_json::from_str::<crate::reports::DiagnosticsReport>(&previous) {
            crate::reports::persist_diagnostics_projection_with_writer_to_path(
                workspace,
                &report,
                diagnostics_target_path.unwrap_or(diagnostics_file()),
                None,
                "diagnostics_rejection_restore",
            )?;
        } else {
            crate::logging::write_projection_with_artifact_effects(
                workspace,
                &diagnostics_path,
                diagnostics_target_path.unwrap_or(diagnostics_file()),
                "write",
                "diagnostics_rejection_restore",
                &previous,
            )?;
        }
    }
    let rejection_msg = format!(
        "apply_patch rejected: DIAGNOSTICS.json is a derived cache view and must match the rendered diagnostics projection from the current workspace issue/violation views.\n\
         Fix: update the underlying issue/violation state instead of editing diagnostics output directly."
    );
    log_error_event(
        role,
        "apply_patch",
        Some(step),
        &rejection_msg,
        Some(json!({
            "stage": "diagnostics_emission_validation",
            "path": diagnostics_path.to_string_lossy(),
        })),
    );
    Ok(Some((false, rejection_msg)))
}

fn validate_schema_guarded_patch_outputs(
    role: &str,
    step: usize,
    workspace: &Path,
    schema_snapshots: &[(String, Option<String>)],
) -> Option<(bool, String)> {
    for (target, prev_content) in schema_snapshots {
        let new_content = fs::read_to_string(workspace.join(target)).unwrap_or_default();
        if let Some(err_msg) = validate_state_file_schema(target, &new_content) {
            return Some(schema_guarded_patch_validation_failure(
                role,
                step,
                workspace,
                target,
                prev_content.as_deref(),
                err_msg,
            ));
        }
    }
    None
}

fn schema_guarded_patch_validation_failure(
    role: &str,
    step: usize,
    workspace: &Path,
    target: &str,
    prev_content: Option<&str>,
    err_msg: String,
) -> (bool, String) {
    if let Some(prev) = prev_content {
        let _ = fs::write(workspace.join(target), prev);
    }
    log_error_event(
        role,
        "apply_patch",
        Some(step),
        &err_msg,
        Some(json!({"stage": "schema_validation", "path": target})),
    );
    (false, err_msg)
}

fn run_patch_crate_verification_command(
    role: &str,
    step: usize,
    workspace: &Path,
    display_cmd: &str,
    cmd: &str,
    ok_label: &'static str,
    fail_label: &'static str,
) -> (bool, String, &'static str) {
    eprintln!("[{role}] step={} {display_cmd}", step);
    let (ok, out) = exec_run_command(workspace, cmd, crate::constants::workspace())
        .unwrap_or_else(|e| (false, e.to_string()));
    let label = if ok { ok_label } else { fail_label };
    eprintln!("[{role}] step={} {label}", step);
    (ok, out, label)
}

fn format_chained_action_entry(
    index: usize,
    action: &str,
    status: &str,
    intent: &str,
    command: Option<&str>,
    result: &str,
) -> String {
    let mut entry =
        format!("{index}. action: {action}\n   status: {status}\n   intent: {intent}\n");
    if let Some(cmd) = command {
        entry.push_str(&format!("   command: {cmd}\n"));
    }
    entry.push_str("   result:\n");
    for line in result.lines() {
        entry.push_str("     ");
        entry.push_str(line);
        entry.push('\n');
    }
    entry.trim_end().to_string()
}

fn format_apply_patch_action_chain(
    check_label: &str,
    check_cmd: &str,
    check_out: &str,
    test_label: Option<&str>,
    test_cmd: Option<&str>,
    test_out: Option<&str>,
    extra_note: Option<&str>,
) -> String {
    let mut sections = vec![
        "apply_patch ok".to_string(),
        "Chained action transcript:".to_string(),
        format_chained_action_entry(
            1,
            "apply_patch",
            "ok",
            "Apply the requested source mutation.",
            None,
            "Patch applied successfully.",
        ),
        format_chained_action_entry(
            2,
            "run_command",
            if check_label.ends_with("ok") {
                "ok"
            } else {
                "failed"
            },
            "Auto-verify the patched crate compiles after the edit.",
            Some(check_cmd),
            truncate(check_out, MAX_SNIPPET),
        ),
    ];

    if let (Some(test_label), Some(test_cmd), Some(test_out)) = (test_label, test_cmd, test_out) {
        sections.push(format_chained_action_entry(
            3,
            "run_command",
            if test_label.ends_with("ok") {
                "ok"
            } else {
                "failed"
            },
            "Auto-verify the patched crate tests after the edit.",
            Some(test_cmd),
            test_out,
        ));
    }

    if let Some(note) = extra_note.filter(|n| !n.trim().is_empty()) {
        sections.push("Notes:".to_string());
        sections.push(note.trim().to_string());
    }

    sections.join("\n\n")
}

fn verification_rebind_note(
    workspace: &Path,
    crate_name: &str,
    plan: &Option<crate::semantic::ExecutionPathPlan>,
    check_out: &str,
    test_out: &str,
) -> String {
    let Some(rebound) =
        verification_rebind(workspace, crate_name, plan.as_ref(), check_out, test_out)
    else {
        return String::new();
    };
    let mut note = String::from("\n\nRebound failure target:\n");
    if let Some(symbol) = rebound.get("symbol").and_then(|v| v.as_str()) {
        note.push_str(&format!("symbol: {symbol}\n"));
    }
    if let (Some(file), Some(line)) = (
        rebound.get("file").and_then(|v| v.as_str()),
        rebound.get("line").and_then(|v| v.as_u64()),
    ) {
        note.push_str(&format!(
            "location: {}:{line}\n",
            crate::semantic::shorten_display_path(file)
        ));
    }
    let rebound_path = execution_plan_rebound_path(workspace, crate_name);
    if rebound_path.exists() {
        note.push_str(&format!(
            "rebound_plan: {}\n",
            crate::semantic::shorten_display_path(&rebound_path.display().to_string())
        ));
    }
    note
}

fn verify_apply_patch_crate(
    role: &str,
    step: usize,
    workspace: &Path,
    patch: &str,
) -> Option<(bool, String)> {
    let patch_targets = patch_targets(patch);
    if !patch_targets_include_rust_sources(&patch_targets) {
        return None;
    }
    let crate_for_patch = patch_first_file(patch).and_then(|f| infer_crate_for_patch(workspace, f));
    let krate = crate_for_patch?;
    let check_cmd = format!("cargo check -p {krate}");

    let (check_ok, check_out, check_label) = run_patch_crate_verification_command(
        role,
        step,
        workspace,
        &check_cmd,
        &check_cmd,
        "cargo check ok",
        "cargo check failed",
    );

    let plan = load_execution_plan(workspace, &krate);
    if !check_ok {
        log_execution_learning(
            workspace,
            &krate,
            patch,
            &plan,
            check_ok,
            &check_out,
            false,
            None,
            "",
        );
        let rebind_note = verification_rebind_note(workspace, &krate, &plan, &check_out, "");
        let out = format_apply_patch_action_chain(
            check_label,
            &check_cmd,
            &check_out,
            None,
            None,
            None,
            Some(&rebind_note),
        );
        return Some((false, out));
    }
    log_execution_learning(
        workspace,
        &krate,
        patch,
        &plan,
        check_ok,
        &check_out,
        false,
        None,
        "",
    );

    Some((
        false,
        format_apply_patch_action_chain(
            check_label,
            &check_cmd,
            &check_out,
            None,
            None,
            None,
            Some("Auto post-patch `cargo test` is disabled; run `cargo_test` explicitly when needed."),
        ),
    ))
}

fn log_execution_learning(
    workspace: &Path,
    crate_name: &str,
    patch: &str,
    plan: &Option<crate::semantic::ExecutionPathPlan>,
    check_ok: bool,
    check_out: &str,
    cargo_test_ran: bool,
    test_ok: Option<bool>,
    test_out: &str,
) {
    let record = execution_learning_record(
        workspace,
        crate_name,
        patch,
        plan,
        check_ok,
        check_out,
        cargo_test_ran,
        test_ok,
        test_out,
    );
    let _ = append_execution_learning_record(workspace, &record);
}

fn execution_learning_record(
    workspace: &Path,
    crate_name: &str,
    patch: &str,
    plan: &Option<crate::semantic::ExecutionPathPlan>,
    check_ok: bool,
    check_out: &str,
    cargo_test_ran: bool,
    test_ok: Option<bool>,
    test_out: &str,
) -> Value {
    let patch_paths = patch_targets(patch)
        .into_iter()
        .map(|path| path.to_string())
        .collect::<Vec<_>>();
    let top_target = plan
        .as_ref()
        .and_then(|value| serde_json::to_value(&value.top_target).ok())
        .and_then(|value| if value.is_null() { None } else { Some(value) });
    let top_target_file = top_target
        .as_ref()
        .and_then(|value| value.get("file"))
        .and_then(|value| value.as_str());
    let matched_top_target = top_target_file
        .map(|file| patch_paths.iter().any(|path| path == file))
        .unwrap_or(false);
    let verified = if cargo_test_ran {
        check_ok && test_ok.unwrap_or(false)
    } else {
        check_ok
    };
    let rebound = if verified {
        None
    } else {
        verification_rebind(workspace, crate_name, plan.as_ref(), check_out, test_out)
    };
    json!({
        "ts_ms": now_ms(),
        "crate": crate_name,
        "path_fingerprint": plan.as_ref().map(|value| value.path_fingerprint.clone()),
        "from": plan.as_ref().map(|value| value.from.clone()),
        "to": plan.as_ref().map(|value| value.to.clone()),
        "top_target": top_target,
        "matched_top_target_file": matched_top_target,
        "patch_paths": patch_paths,
        "patch_kind": "apply_patch",
        "verification": {
            "cargo_check_ok": check_ok,
            "cargo_test_ran": cargo_test_ran,
            "cargo_test_ok": test_ok,
            "verified": verified,
        },
        "rebound_failure": rebound,
        "check_excerpt": truncate(check_out, MAX_SNIPPET),
        "test_excerpt": truncate(test_out, MAX_SNIPPET),
    })
}

fn append_python_failure_guidance(out: &mut String, cwd: &str, workspace: &Path) {
    let lowered = out.to_lowercase();
    let mut context = String::new();
    if !PathBuf::from(cwd).starts_with(workspace) && !cwd.starts_with("/tmp") {
        context.push_str(&format!(
            "python cwd escapes workspace; set cwd to {} or /tmp.\n",
            crate::constants::workspace()
        ));
    }
    if !context.is_empty() {
        out.push('\n');
        out.push_str(context.trim_end());
    }
    if lowered.contains("permission denied") || lowered.contains("errno 13") {
        out.push_str(
            &format!("\npython write denied: verify the target path is under {ws} and set cwd={ws}; if still blocked, use apply_patch for `src/`, `PLAN.json`, or lane plan edits.", ws = crate::constants::workspace()),
        );
    }
}

fn python_action_label(success: bool) -> &'static str {
    if success {
        "python ok"
    } else {
        "python failed"
    }
}

fn handle_apply_patch_action(
    role: &str,
    step: usize,
    mut writer: Option<&mut CanonicalWriter>,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let patch = action
        .get("patch")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("apply_patch missing 'patch'"))?;
    let diagnostics_target_path = apply_patch_diagnostics_target_path(role, patch);
    let diagnostics_targeted = diagnostics_target_path.is_some();
    let previous_diagnostics_text =
        previous_diagnostics_patch_text(workspace, diagnostics_target_path.as_deref());
    let schema_snapshots = schema_guard_snapshots_for_patch(workspace, patch);
    if let Some(msg) = patch_scope_error(role, &patch) {
        return Ok((false, msg));
    }
    let patch_targets = patch_targets(patch);
    request_rust_patch_verification_if_needed(&patch_targets, writer.as_deref_mut());
    let patch_signature = artifact_write_signature(&[
        "apply_patch",
        role,
        &step.to_string(),
        &patch_targets.join(","),
        &patch.len().to_string(),
    ]);
    if let Some(writer) = writer.as_mut() {
        try_emit_workspace_artifact_effect(
            writer,
            true,
            "apply_patch",
            "apply",
            patch_targets.first().copied().unwrap_or("(unknown)"),
            role,
            &patch_signature,
        )?;
    }
    let snapshots = snapshot_patch_targets(workspace, &patch_targets)?;
    match apply_patch(&patch, workspace) {
        Ok(affected) => handle_apply_patch_success(
            role,
            step,
            workspace,
            patch,
            diagnostics_targeted,
            diagnostics_target_path.as_deref(),
            previous_diagnostics_text,
            &schema_snapshots,
            &mut writer,
            &snapshots,
            &affected,
            &patch_signature,
        ),
        Err(e) => handle_apply_patch_failure(role, step, workspace, patch, &e.to_string()),
    }
}

fn handle_apply_patch_success(
    role: &str,
    step: usize,
    workspace: &Path,
    patch: &str,
    diagnostics_targeted: bool,
    diagnostics_target_path: Option<&str>,
    previous_diagnostics_text: Option<String>,
    schema_snapshots: &[(String, Option<String>)],
    writer: &mut Option<&mut CanonicalWriter>,
    snapshots: &std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
    affected: &crate::canon_tools_patch::AffectedPaths,
    patch_signature: &str,
) -> Result<(bool, String)> {
    if let Some(result) = reject_unvalidated_diagnostics_persistence(
        role,
        step,
        workspace,
        diagnostics_targeted,
        diagnostics_target_path,
        previous_diagnostics_text,
    )? {
        return Ok(result);
    }
    if let Some(result) =
        validate_schema_guarded_patch_outputs(role, step, workspace, schema_snapshots)
    {
        return Ok(result);
    }
    eprintln!("[{role}] step={} apply_patch ok", step);
    if let Some(result) = verify_apply_patch_crate(role, step, workspace, patch) {
        if let Some(writer) = writer.as_mut() {
            let target = affected
                .modified
                .first()
                .or_else(|| affected.added.first())
                .or_else(|| affected.deleted.first())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "(unknown)".to_string());
            try_emit_workspace_artifact_effect(
                writer,
                false,
                "apply_patch",
                "apply",
                &target,
                role,
                patch_signature,
            )?;
        }
        return Ok(result);
    }
    if let Some(writer) = writer.as_mut() {
        let target = affected
            .modified
            .first()
            .or_else(|| affected.added.first())
            .or_else(|| affected.deleted.first())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(unknown)".to_string());
        if let Err(err) = try_emit_workspace_artifact_effect(
            writer,
            false,
            "apply_patch",
            "apply",
            &target,
            role,
            patch_signature,
        ) {
            restore_patch_snapshots(snapshots)?;
            return Err(err);
        }
    }
    // Enrich the success result with the post-patch file content so the agent can
    // verify the change without a separate read_file round-trip.
    let post_patch_snippet = patch_first_file(patch)
        .and_then(|rel| safe_join(workspace, rel).ok())
        .and_then(|abs| std::fs::read_to_string(&abs).ok())
        .map(|content| {
            let lines: Vec<String> = content
                .lines()
                .take(80)
                .enumerate()
                .map(|(i, l)| format!("{}: {}", i + 1, l))
                .collect();
            let truncated = if content.lines().count() > 80 {
                "\n... (truncated at 80 lines)"
            } else {
                ""
            };
            let rel = patch_first_file(patch).unwrap_or("patched file");
            format!(
                "\nPost-patch content of {rel} (first 80 lines):\n{}{}",
                lines.join("\n"),
                truncated
            )
        })
        .unwrap_or_default();
    Ok((false, format!("apply_patch ok{post_patch_snippet}")))
}

fn snapshot_patch_targets(
    workspace: &Path,
    targets: &[&str],
) -> Result<std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>> {
    let mut snapshots = std::collections::BTreeMap::new();
    for target in targets {
        let path = safe_join(workspace, target)?;
        snapshots.insert(path.clone(), file_snapshot(&path)?);
    }
    Ok(snapshots)
}

fn restore_patch_snapshots(
    snapshots: &std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> Result<()> {
    snapshots
        .iter()
        .rev()
        .try_for_each(|(path, snapshot)| restore_file_snapshot(path, snapshot))
}

fn handle_apply_patch_failure(
    role: &str,
    step: usize,
    workspace: &Path,
    patch: &str,
    err_str: &str,
) -> Result<(bool, String)> {
    eprintln!("[{role}] step={} apply_patch failed: {err_str}", step);
    log_apply_patch_failure(role, step, patch, err_str);
    let read_path = extract_anchor_fail_path(err_str)
        .or_else(|| patch_first_file(patch).map(|s| s.to_string()));
    let guidance = patch_failure_guidance(read_path.as_deref(), err_str);
    let mut msg = format!("apply_patch failed: {err_str}\n\n{guidance}");
    if let Some(fp) = read_path {
        if let Ok(content) = auto_read_for_patch_anchor(workspace, &fp, err_str) {
            eprintln!("[{role}] step={} auto_read path={fp}", step);
            msg = format!("apply_patch failed: {err_str}\n\n{guidance}\n\n{content}");
        }
    }
    Ok((false, msg))
}

fn log_apply_patch_failure(role: &str, step: usize, patch: &str, err_str: &str) {
    log_error_event(
        role,
        "apply_patch",
        Some(step),
        &format!("apply_patch failed: {err_str}"),
        patch_first_file(patch).map(|path| {
            json!({
                "stage": "apply_patch",
                "path": path,
            })
        }),
    );
}

fn handle_run_command_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let cmd = action
        .get("cmd")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("run_command missing 'cmd'"))?;
    let cwd = action
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::constants::workspace());
    eprintln!("[{role}] step={} run_command cmd={cmd}", step);
    let (success, out) = exec_run_command(workspace, cmd, cwd)?;
    let receipt_id = append_evidence_receipt(
        role,
        step,
        "run_command",
        None,
        Some(PathBuf::from(cwd)),
        json!({"cmd": cmd, "success": success}),
        &out,
    )
    .ok();
    let label = if success {
        "run_command ok"
    } else {
        "run_command failed"
    };
    eprintln!("[{role}] step={} {label} output_bytes={}", step, out.len());
    if !success {
        log_error_event(
            role,
            "run_command",
            Some(step),
            &format!("run_command failed: {cmd}"),
            Some(json!({
                "stage": "run_command",
                "cmd": cmd,
                "cwd": cwd,
            })),
        );
    }
    Ok((
        false,
        format_output_with_evidence_receipt(
            label,
            &truncate(&out, MAX_SNIPPET),
            receipt_id.as_deref(),
        ),
    ))
}

fn handle_python_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let code = action
        .get("code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("python missing 'code'"))?;
    let cwd = action
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::constants::workspace());
    eprintln!("[{role}] step={} python bytes={}", step, code.len());
    let (success, mut out) = exec_python(workspace, code, cwd)?;
    let receipt_id = append_evidence_receipt(
        role,
        step,
        "python",
        None,
        Some(PathBuf::from(cwd)),
        json!({"success": success, "code_hash": stable_hash_hex(code)}),
        &out,
    )
    .ok();
    if !success {
        append_python_failure_guidance(&mut out, cwd, workspace);
    }
    let label = python_action_label(success);
    eprintln!("[{role}] step={} {label} output_bytes={}", step, out.len());
    if !success {
        log_error_event(
            role,
            "python",
            Some(step),
            "python action failed",
            Some(json!({
                "stage": "python",
                "cwd": cwd,
            })),
        );
    }
    Ok((
        false,
        format_output_with_evidence_receipt(
            label,
            &truncate(&out, MAX_SNIPPET),
            receipt_id.as_deref(),
        ),
    ))
}

fn handle_rustc_action(
    role: &str,
    step: usize,
    action_kind: &str,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let crate_name = action
        .get("crate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{action_kind} missing 'crate'"))?;
    let mode =
        action
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or(if action_kind == "rustc_hir" {
                "hir-tree"
            } else {
                "mir"
            });
    let extra = action.get("extra").and_then(|v| v.as_str()).unwrap_or("");

    // Preferred path: use the canonical `state/rustc/<crate>/graph.json` artifact (canon-rustc-v2).
    // This avoids relying on `-Zunpretty` output, and works even when the project uses a non-standard rustc wrapper.
    if let Ok(out) =
        graph_backed_rustc_action_output(workspace, action_kind, crate_name, mode, extra, action)
    {
        eprintln!(
            "[{role}] step={} {action_kind} graph={} output_bytes={}",
            step,
            workspace
                .join("state/rustc")
                .join(crate_name.replace('-', "_"))
                .join("graph.json")
                .display(),
            out.len()
        );
        return Ok((false, truncate(&out, MAX_SNIPPET).to_string()));
    }

    fallback_rustc_action(role, step, action_kind, workspace, crate_name, mode, extra)
}

fn build_fallback_rustc_command(crate_name: &str, mode: &str, extra: &str) -> String {
    if extra.trim().is_empty() {
        format!("cargo rustc -p {crate_name} -- -Zunpretty={mode}")
    } else {
        format!("cargo rustc -p {crate_name} -- -Zunpretty={mode} {extra}")
    }
}

fn fallback_rustc_action_label(action_kind: &str, success: bool) -> String {
    if success {
        format!("{action_kind} ok")
    } else {
        format!("{action_kind} failed")
    }
}

fn log_fallback_rustc_failure(
    role: &str,
    step: usize,
    action_kind: &str,
    crate_name: &str,
    mode: &str,
    extra: &str,
    cmd: &str,
) {
    log_error_event(
        role,
        action_kind,
        Some(step),
        &format!("{action_kind} failed for crate {crate_name}"),
        Some(json!({
            "stage": action_kind,
            "crate": crate_name,
            "mode": mode,
            "extra": extra,
            "cmd": cmd,
        })),
    );
}

fn format_fallback_rustc_output(label: &str, out: &str, crate_name: &str) -> String {
    format!(
        "{label}:\n{}",
        truncate(
            &format!(
                "{out}\n\nnote: state/rustc/{}/graph.json not available; build with canon-rustc-v2 wrapper to enable graph-backed rustc_hir/rustc_mir output.",
                crate_name.replace('-', "_")
            ),
            MAX_SNIPPET
        )
    )
}

fn fallback_rustc_action(
    role: &str,
    step: usize,
    action_kind: &str,
    workspace: &Path,
    crate_name: &str,
    mode: &str,
    extra: &str,
) -> Result<(bool, String)> {
    let cmd = build_fallback_rustc_command(crate_name, mode, extra);
    eprintln!("[{role}] step={} {action_kind} cmd={cmd}", step);
    let (success, out) = exec_run_command(workspace, &cmd, crate::constants::workspace())?;
    let label = fallback_rustc_action_label(action_kind, success);
    eprintln!("[{role}] step={} {label} output_bytes={}", step, out.len());
    if !success {
        log_fallback_rustc_failure(role, step, action_kind, crate_name, mode, extra, &cmd);
    }
    Ok((
        false,
        format_fallback_rustc_output(&label, &out, crate_name),
    ))
}

fn graph_backed_rustc_action_output(
    workspace: &Path,
    action_kind: &str,
    crate_name: &str,
    mode: &str,
    extra: &str,
    action: &Value,
) -> Result<String> {
    let idx = crate::semantic::SemanticIndex::load(workspace, crate_name)?;
    let crate_norm = crate_name.replace('-', "_");
    let graph_path = workspace
        .join("state/rustc")
        .join(&crate_norm)
        .join("graph.json");
    let symbol = rustc_action_symbol(action, &crate_norm);
    let filter = parse_rustc_graph_filter(extra.trim());
    if action_kind == "rustc_hir" {
        return Ok(format_graph_backed_hir_output(
            &idx,
            &graph_path,
            mode,
            symbol.as_deref(),
            filter.as_deref(),
        ));
    }
    Ok(format_graph_backed_mir_output(
        &idx,
        &graph_path,
        mode,
        symbol.as_deref(),
        filter.as_deref(),
    )?)
}

fn rustc_action_symbol(action: &Value, crate_norm: &str) -> Option<String> {
    let symbol_raw = action
        .get("symbol")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if symbol_raw.is_empty() {
        None
    } else {
        Some(strip_semantic_crate_prefix(crate_norm, symbol_raw).to_string())
    }
}

fn format_graph_backed_hir_output(
    idx: &crate::semantic::SemanticIndex,
    graph_path: &Path,
    mode: &str,
    symbol: Option<&str>,
    filter: Option<&str>,
) -> String {
    let map = if let Some(sym) = symbol {
        match idx.symbol_window(sym) {
            Ok(body) => body,
            Err(e) => format!("symbol_window failed for {sym}: {e}"),
        }
    } else {
        idx.semantic_map(filter, false)
    };
    format!(
        "rustc_hir ok (graph):\nsource: {}\nmode: {}\nsymbol: {}\nfilter: {}\n\n{}",
        graph_path.display(),
        mode,
        symbol.unwrap_or(""),
        filter.unwrap_or(""),
        map.trim_end()
    )
}

fn format_graph_backed_mir_output(
    idx: &crate::semantic::SemanticIndex,
    graph_path: &Path,
    mode: &str,
    symbol: Option<&str>,
    filter: Option<&str>,
) -> Result<String> {
    if let Some(sym) = symbol {
        return format_graph_backed_mir_symbol_output(idx, graph_path, mode, sym);
    }
    Ok(format_graph_backed_mir_listing_output(
        idx, graph_path, mode, filter,
    ))
}

fn format_graph_backed_mir_symbol_output(
    idx: &crate::semantic::SemanticIndex,
    graph_path: &Path,
    mode: &str,
    sym: &str,
) -> Result<String> {
    let canonical = idx.canonical_symbol_key(sym)?;
    let all = idx.symbol_summaries();
    let mut mir_items: Vec<&crate::semantic::SymbolSummary> =
        all.iter().filter(|s| s.mir_fingerprint.is_some()).collect();
    mir_items.sort_by(|a, b| b.mir_blocks.unwrap_or(0).cmp(&a.mir_blocks.unwrap_or(0)));
    let total = mir_items.len().max(1);
    let rank = mir_items
        .iter()
        .position(|s| s.symbol == canonical)
        .map(|i| i + 1);
    let summary = all.iter().find(|s| s.symbol == canonical);
    let mut body = String::new();
    if let Some(s) = summary {
        let fp = s
            .mir_fingerprint
            .clone()
            .unwrap_or_else(|| "none".to_string());
        let blocks = s.mir_blocks.unwrap_or(0);
        let stmts = s.mir_stmts.unwrap_or(0);
        body.push_str(&format!(
            "symbol: {sym} -> {canonical}\nfile: {}:{}\nmir: fingerprint={fp} blocks={blocks} stmts={stmts}\nrank_by_blocks: {}/{}\ncalls: in={} out={}",
            crate::semantic::shorten_display_path(&s.file),
            s.line,
            rank.unwrap_or(0),
            total,
            s.call_in,
            s.call_out
        ));
    } else {
        body.push_str(&format!("symbol: {sym} -> {canonical}\nmir: none\n"));
    }
    Ok(format!(
        "rustc_mir ok (graph):\nsource: {}\nmode: {}\n\n{}",
        graph_path.display(),
        mode,
        body.trim_end()
    ))
}

fn format_graph_backed_mir_listing_output(
    idx: &crate::semantic::SemanticIndex,
    graph_path: &Path,
    mode: &str,
    filter: Option<&str>,
) -> String {
    let mut summaries = idx.symbol_summaries();
    if let Some(prefix) = filter.filter(|s| !s.is_empty()) {
        summaries.retain(|s| s.symbol.starts_with(prefix));
    }
    summaries.retain(|s| s.mir_fingerprint.is_some());
    let mut body = String::new();
    for s in summaries {
        let fp = s.mir_fingerprint.unwrap_or_default();
        let blocks = s.mir_blocks.unwrap_or(0);
        let stmts = s.mir_stmts.unwrap_or(0);
        body.push_str(&format!(
            "{}:{} {}  mir(fp={}, blocks={}, stmts={})\n",
            crate::semantic::shorten_display_path(&s.file),
            s.line,
            s.symbol,
            fp,
            blocks,
            stmts
        ));
    }
    if body.trim().is_empty() {
        body.push_str("(no MIR metadata entries found in graph)\n");
    }
    format!(
        "rustc_mir ok (graph):\nsource: {}\nmode: {}\nfilter: {}\n\n{}",
        graph_path.display(),
        mode,
        filter.unwrap_or(""),
        body.trim_end()
    )
}

fn parse_rustc_graph_filter(extra: &str) -> Option<String> {
    let s = extra.trim();
    if s.is_empty() {
        return None;
    }
    // Accept a few simple conventions so existing prompts can steer the output:
    //   --symbol=foo::bar
    //   --filter=foo::bar
    //   --path=foo::bar
    // Otherwise treat the full `extra` string as the filter prefix.
    for key in ["--symbol=", "--filter=", "--path="] {
        if let Some(rest) = s.strip_prefix(key) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    Some(s.to_string())
}

fn cargo_test_totals_summary(out: &str) -> String {
    let mut kept = Vec::new();
    for line in out.lines() {
        let t = line.trim_end();
        if t.starts_with("running ")
            || t.starts_with("test result:")
            || t.starts_with("Doc-tests ")
            || t.starts_with("running unittests ")
        {
            kept.push(t.to_string());
        }
    }
    kept.join("\n")
}

fn write_state_log(workspace: &Path, tool: &str, content: &str) -> Result<String> {
    let safe_tool = tool
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>();
    let dir = workspace.join("state").join("logs").join(safe_tool);
    std::fs::create_dir_all(&dir).with_context(|| format!("create log dir {}", dir.display()))?;
    let ts = now_ms();
    let path = dir.join(format!("{ts}.log"));
    std::fs::write(&path, content).with_context(|| format!("write log {}", path.display()))?;
    Ok(path
        .strip_prefix(workspace)
        .unwrap_or(&path)
        .to_string_lossy()
        .to_string())
}

fn summarize_compiler_like_output(out: &str) -> String {
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut first: Option<String> = None;
    for line in out.lines() {
        let t = line.trim_start();
        if t.starts_with("error:") {
            errors += 1;
            if first.is_none() {
                first = Some(t.to_string());
            }
        } else if t.starts_with("warning:") {
            warnings += 1;
            if first.is_none() {
                first = Some(t.to_string());
            }
        }
    }
    let mut summary = format!("errors={errors} warnings={warnings}");
    if let Some(first) = first {
        let mut first = first.replace('\n', " ");
        if first.len() > 160 {
            first.truncate(160);
            first.push_str("…");
        }
        summary.push_str(&format!(" first={first}"));
    }
    summary
}

fn handle_graph_call_cfg_action(
    role: &str,
    step: usize,
    action_kind: &str,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let (crate_name, out_dir) = parse_graph_call_cfg_action_input(action_kind, workspace, action)?;
    execute_graph_call_cfg_action(role, step, action_kind, workspace, crate_name, out_dir)
}

fn execute_graph_call_cfg_action(
    role: &str,
    step: usize,
    action_kind: &str,
    workspace: &Path,
    crate_name: &str,
    out_dir: PathBuf,
) -> Result<(bool, String)> {
    let out_dir_str = out_dir.to_string_lossy().to_string();
    let artifact_crate = ensure_graph_artifact(workspace, crate_name, role, step)?;
    let (bin_ok, bin_out, bin_label) = run_graph_call_cfg_bin(
        role,
        step,
        action_kind,
        workspace,
        crate_name,
        &artifact_crate,
        &out_dir_str,
    )?;
    let output = render_graph_call_cfg_output(
        action_kind,
        &out_dir,
        &out_dir_str,
        bin_ok,
        &bin_out,
        bin_label,
    )?;
    Ok((false, output))
}

fn parse_graph_call_cfg_action_input<'a>(
    action_kind: &'a str,
    workspace: &'a Path,
    action: &'a Value,
) -> Result<(&'a str, PathBuf)> {
    let crate_name = action
        .get("crate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{action_kind} missing 'crate'"))?;
    let out_dir = action
        .get("out_dir")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_graph_out_dir(workspace, crate_name));
    Ok((crate_name, out_dir))
}

fn build_graph_call_cfg_command(artifact_crate: &str, out_dir_str: &str) -> String {
    format!(
        "cargo run -p canon-tools-analysis --bin graph_bin -- --workspace {} --crate {} --out {}",
        crate::constants::workspace(),
        artifact_crate,
        out_dir_str
    )
}

fn graph_call_cfg_bin_label(bin_ok: bool) -> &'static str {
    if bin_ok {
        "graph_bin ok"
    } else {
        "graph_bin failed"
    }
}

fn graph_call_cfg_action_label(action_kind: &str, bin_ok: bool) -> String {
    if bin_ok {
        format!("{action_kind} ok")
    } else {
        format!("{action_kind} failed")
    }
}

fn run_graph_call_cfg_bin(
    role: &str,
    step: usize,
    action_kind: &str,
    workspace: &Path,
    crate_name: &str,
    artifact_crate: &str,
    out_dir_str: &str,
) -> Result<(bool, String, &'static str)> {
    let bin_cmd = build_graph_call_cfg_command(artifact_crate, out_dir_str);
    eprintln!("[{role}] step={} graph_bin cmd={bin_cmd}", step);
    let (bin_ok, bin_out) = exec_graph_command(workspace, &bin_cmd)?;
    let bin_label = graph_call_cfg_bin_label(bin_ok);
    eprintln!(
        "[{role}] step={} {bin_label} output_bytes={}",
        step,
        bin_out.len()
    );
    if !bin_ok {
        log_graph_call_cfg_failure(
            role,
            step,
            action_kind,
            crate_name,
            artifact_crate,
            &bin_cmd,
            out_dir_str,
        );
    }
    Ok((bin_ok, bin_out, bin_label))
}

fn render_graph_call_cfg_output(
    action_kind: &str,
    out_dir: &Path,
    out_dir_str: &str,
    bin_ok: bool,
    bin_out: &str,
    bin_label: &str,
) -> Result<String> {
    let label = graph_call_cfg_action_label(action_kind, bin_ok);
    let target_path = graph_call_cfg_target_path(out_dir, action_kind);
    let (preview, symbol_preview, symbol_path) =
        collect_graph_call_cfg_preview_data(out_dir, &target_path, action_kind)?;
    let summary = build_graph_call_cfg_summary(
        &label,
        out_dir_str,
        &target_path,
        &preview,
        symbol_preview.as_str(),
        symbol_path.as_ref(),
    );
    Ok(format_graph_call_cfg_output(&summary, bin_label, bin_out))
}

fn collect_graph_call_cfg_preview_data(
    out_dir: &Path,
    target_path: &Path,
    action_kind: &str,
) -> Result<(String, String, Option<PathBuf>)> {
    let preview = graph_preview_text(target_path)?;
    let (symbol_preview, symbol_path) =
        build_graph_symbol_preview(out_dir, target_path, action_kind)?;
    Ok((preview, symbol_preview, symbol_path))
}

fn format_graph_call_cfg_output(summary: &str, bin_label: &str, bin_out: &str) -> String {
    let full_out = format!("{bin_label}:\n{}\n", truncate(bin_out, MAX_SNIPPET));
    format!("{summary}\n\nfull_output:\n{full_out}")
}

fn log_graph_call_cfg_failure(
    role: &str,
    step: usize,
    action_kind: &str,
    crate_name: &str,
    artifact_crate: &str,
    bin_cmd: &str,
    out_dir_str: &str,
) {
    log_error_event(
        role,
        action_kind,
        Some(step),
        &format!("graph_bin failed for crate {crate_name}"),
        Some(json!({
            "stage": action_kind,
            "crate": crate_name,
            "artifact_crate": artifact_crate,
            "cmd": bin_cmd,
            "out_dir": out_dir_str.to_string(),
        })),
    );
}

fn build_graph_call_cfg_summary(
    label: &str,
    out_dir_str: &str,
    target_path: &Path,
    preview: &str,
    symbol_preview: &str,
    symbol_path: Option<&PathBuf>,
) -> String {
    let mut summary = format!(
        "{label}\noutput_dir: {}\n{}",
        out_dir_str,
        target_path.display()
    );
    if !preview.is_empty() {
        summary.push_str("\npreview:\n");
        summary.push_str(preview);
    }
    if let Some(path) = symbol_path {
        summary.push_str(&format!("\nsymbol_edges: {}", path.display()));
        if !symbol_preview.is_empty() {
            summary.push_str("\nsymbol_preview:\n");
            summary.push_str(symbol_preview);
        }
    }
    summary
}

fn graph_call_cfg_target_path(out_dir: &Path, action_kind: &str) -> PathBuf {
    out_dir
        .join("graphs")
        .join(graph_call_cfg_filename(action_kind))
}

fn graph_call_cfg_filename(action_kind: &str) -> &'static str {
    if action_kind == "graph_call" {
        "callgraph.csv"
    } else {
        "cfg.csv"
    }
}

fn graph_preview_text(target_path: &Path) -> Result<String> {
    if target_path.exists() {
        read_first_lines(target_path, 50, MAX_SNIPPET)
    } else {
        Ok(String::new())
    }
}

fn build_graph_symbol_preview(
    out_dir: &Path,
    target_path: &Path,
    action_kind: &str,
) -> Result<(String, Option<PathBuf>)> {
    if !target_path.exists() {
        return Ok((String::new(), None));
    }
    let out_lines = graph_symbol_preview_lines(out_dir, target_path)?;
    if out_lines.is_empty() {
        return Ok((String::new(), None));
    }
    let symbol_preview = out_lines.join("\n");
    let out_path = write_graph_symbol_preview_file(out_dir, action_kind, &symbol_preview)?;
    Ok((symbol_preview, Some(out_path)))
}

fn graph_symbol_preview_lines(out_dir: &Path, target_path: &Path) -> Result<Vec<String>> {
    let content = fs::read_to_string(target_path)?;
    let mut lines = content.lines();
    let header = lines.next().unwrap_or("");
    let header_cols: Vec<&str> = header.split(',').collect();
    let has_symbol_cols = header_cols
        .iter()
        .any(|c| *c == "caller_symbol" || *c == "callee_symbol");
    let map = graph_symbol_map(out_dir, has_symbol_cols)?;
    let mut out_lines = Vec::new();
    let mut count = 0usize;
    for line in lines {
        if count >= 200 {
            break;
        }
        if let Some(symbol_edge) = graph_symbol_edge_from_columns(&header_cols, line) {
            out_lines.push(symbol_edge);
            count += 1;
            continue;
        }
        if let Some(mapped_edge) = graph_symbol_edge_from_ids(map.as_ref(), line) {
            out_lines.push(mapped_edge);
            count += 1;
        }
    }
    Ok(out_lines)
}

fn graph_symbol_map(
    out_dir: &Path,
    has_symbol_cols: bool,
) -> Result<Option<std::collections::HashMap<u32, (String, String)>>> {
    if has_symbol_cols {
        return Ok(None);
    }
    let graph_json = out_dir.join("graph").join("graph.json");
    if graph_json.exists() {
        return Ok(Some(load_graph_symbols(&graph_json)?));
    }
    let nodes_csv = out_dir.join("graph").join("nodes.csv");
    if nodes_csv.exists() {
        return Ok(Some(load_nodes_symbols(&nodes_csv)?));
    }
    Ok(None)
}

fn graph_symbol_edge_from_columns(header_cols: &[&str], line: &str) -> Option<String> {
    let cols: Vec<&str> = line.split(',').collect();
    let caller_idx = header_cols.iter().position(|c| *c == "caller_symbol")?;
    let callee_idx = header_cols.iter().position(|c| *c == "callee_symbol")?;
    let caller = cols.get(caller_idx).map(|s| s.trim()).unwrap_or("");
    let callee = cols.get(callee_idx).map(|s| s.trim()).unwrap_or("");
    if caller.is_empty() && callee.is_empty() {
        None
    } else {
        Some(format!("{caller} -> {callee}"))
    }
}

fn graph_symbol_edge_from_ids(
    map: Option<&std::collections::HashMap<u32, (String, String)>>,
    line: &str,
) -> Option<String> {
    let cols: Vec<&str> = line.split(',').collect();
    if cols.len() < 2 {
        return None;
    }
    let src = cols[0].trim();
    let dst = cols[1].trim();
    Some(if let Some(map) = map {
        format!("{} -> {}", symbol_label(map, src), symbol_label(map, dst))
    } else {
        format!("{src} -> {dst}")
    })
}

fn write_graph_symbol_preview_file(
    out_dir: &Path,
    action_kind: &str,
    symbol_preview: &str,
) -> Result<PathBuf> {
    let fname = if action_kind == "graph_call" {
        "callgraph.symbol.txt"
    } else {
        "cfg.symbol.txt"
    };
    let out_path = out_dir.join("graphs").join(fname);
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&out_path, format!("{}\n", symbol_preview))?;
    Ok(out_path)
}

fn parse_graph_reports_action_input(
    action_kind: &str,
    workspace: &Path,
    action: &Value,
) -> Result<(String, Option<String>, PathBuf)> {
    let crate_name = action
        .get("crate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{action_kind} missing 'crate'"))?
        .to_string();
    let tlog = action
        .get("tlog")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let out_dir = action
        .get("out_dir")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_graph_out_dir(workspace, &crate_name));
    Ok((crate_name, tlog, out_dir))
}

fn build_graph_reports_command(
    artifact_crate: &str,
    out_dir_str: &str,
    tlog: Option<&str>,
) -> String {
    let mut cmd = format!(
        "cargo run -p canon-tools-analysis --bin graph_reports -- --workspace {} --crate {} --out {} --artifact",
        crate::constants::workspace(), artifact_crate, out_dir_str
    );
    if let Some(path) = tlog {
        cmd.push_str(&format!(" --tlog {path}"));
    }
    cmd
}

fn log_graph_reports_failure(
    role: &str,
    step: usize,
    action_kind: &str,
    crate_name: &str,
    artifact_crate: &str,
    cmd: &str,
    out_dir_str: &str,
    tlog: Option<&str>,
) {
    log_error_event(
        role,
        action_kind,
        Some(step),
        &format!("{action_kind} failed for crate {crate_name}"),
        Some(json!({
            "stage": action_kind,
            "crate": crate_name,
            "artifact_crate": artifact_crate,
            "cmd": cmd,
            "out_dir": out_dir_str.to_string(),
            "tlog": tlog,
        })),
    );
}

fn graph_report_path(crate_dir: &Path, action_kind: &str) -> (PathBuf, &'static str) {
    if action_kind == "graph_dataflow" {
        (
            crate_dir
                .join("metrics")
                .join("dataflow_fanout_report.json"),
            "dataflow_fanout_report.json",
        )
    } else {
        let runtime_path = crate_dir
            .join("analysis")
            .join("runtime_reachability_report.json");
        if runtime_path.exists() {
            (runtime_path, "runtime_reachability_report.json")
        } else {
            (
                crate_dir.join("metrics").join("reachability_report.json"),
                "reachability_report.json",
            )
        }
    }
}

fn build_graph_reports_summary(
    label: &str,
    out_dir_str: &str,
    report_path: &Path,
    report_label: &str,
) -> Result<String> {
    let report_preview = if report_path.exists() {
        read_json_report(report_path, MAX_SNIPPET)?
    } else {
        String::new()
    };
    let mut summary = format!(
        "{label}\noutput_dir: {}\nreport: {}",
        out_dir_str,
        report_path.display()
    );
    if !report_preview.is_empty() {
        summary.push_str("\nreport_preview:\n");
        summary.push_str(&report_preview);
    } else {
        summary.push_str(&format!("\nreport_note: {} not found", report_label));
    }
    Ok(summary)
}

fn handle_graph_reports_action(
    role: &str,
    step: usize,
    action_kind: &str,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let (crate_name, tlog, out_dir) =
        parse_graph_reports_action_input(action_kind, workspace, action)?;
    let artifact_crate = ensure_graph_artifact(workspace, &crate_name, role, step)?;
    let out_dir_str = out_dir.to_string_lossy().to_string();
    let crate_dir = report_crate_dir(&out_dir, &crate_name);
    let cmd = build_graph_reports_command(&artifact_crate, &out_dir_str, tlog.as_deref());
    eprintln!("[{role}] step={} {action_kind} cmd={cmd}", step);
    let (success, out) = exec_graph_command(workspace, &cmd)?;
    let label = graph_reports_action_label(action_kind, success);
    maybe_log_graph_reports_failure(
        success,
        role,
        step,
        action_kind,
        &crate_name,
        &artifact_crate,
        &cmd,
        &out_dir_str,
        tlog.as_deref(),
    );
    Ok((
        false,
        build_graph_reports_output(
            &label,
            action_kind,
            &out_dir_str,
            &crate_dir,
            &out,
        )?,
    ))
}

fn build_graph_reports_output(
    label: &str,
    action_kind: &str,
    out_dir_str: &str,
    crate_dir: &Path,
    out: &str,
) -> Result<String> {
    let (report_path, report_label) = graph_report_path(crate_dir, action_kind);
    let summary = build_graph_reports_summary(label, out_dir_str, &report_path, report_label)?;
    Ok(format!(
        "{summary}\n\nfull_output:\n{}",
        truncate(out, MAX_SNIPPET)
    ))
}

fn graph_reports_action_label(action_kind: &str, success: bool) -> String {
    if success {
        format!("{action_kind} ok")
    } else {
        format!("{action_kind} failed")
    }
}

fn maybe_log_graph_reports_failure(
    success: bool,
    role: &str,
    step: usize,
    action_kind: &str,
    crate_name: &str,
    artifact_crate: &str,
    cmd: &str,
    out_dir_str: &str,
    tlog: Option<&str>,
) {
    if !success {
        log_graph_reports_failure(
            role,
            step,
            action_kind,
            crate_name,
            artifact_crate,
            cmd,
            out_dir_str,
            tlog,
        );
    }
}

fn build_cargo_test_command(crate_name: &str, test_name: Option<&str>) -> String {
    if let Some(test_name) = test_name {
        format!("cargo test -q -p {} {} -- --exact", crate_name, test_name)
    } else {
        // Faster default profile: skip doc tests and suppress noisy output.
        // Callers can still target explicit tests via `test`.
        format!("cargo test -q -p {} --lib --bins --tests", crate_name)
    }
}

fn load_cached_failed_tests(workspace: &Path) -> Vec<String> {
    let path = workspace.join("cargo_test_failures.json");
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    value
        .get("failed_tests")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .take(6)
        .collect()
}

fn build_cached_failed_tests_command(crate_name: &str, tests: &[String]) -> Option<String> {
    if tests.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for test in tests {
        parts.push(format!(
            "cargo test -q -p {} {} -- --exact",
            crate_name, test
        ));
    }
    Some(parts.join(" && "))
}

fn cargo_test_summary_line(log_path: Option<&Path>, out: &str) -> Option<String> {
    log_path.and_then(summarize_cargo_test_log).or_else(|| {
        cargo_test_totals_summary(out)
            .lines()
            .next()
            .map(|s| s.to_string())
    })
}

fn summarize_cargo_test_log(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    if contents.trim().is_empty() {
        return None;
    }
    for line in contents.lines() {
        if let Some(idx) = line.find("test result:") {
            return Some(line[idx..].trim().to_string());
        }
    }
    None
}

fn cargo_test_label(summary_line: Option<&str>, spawn_ok: bool) -> &'static str {
    if let Some(summary) = summary_line {
        if summary.contains("test result: ok.") {
            "cargo_test ok"
        } else if summary.contains("test result: FAILED") {
            "cargo_test failed"
        } else {
            "cargo_test running"
        }
    } else if spawn_ok {
        "cargo_test running"
    } else {
        "cargo_test failed"
    }
}

fn append_cargo_test_failure_section(
    summary: &mut String,
    failures_json: &Value,
    key: &str,
    header: &str,
    bullet_prefix: bool,
) {
    let Some(arr) = failures_json.get(key).and_then(|v| v.as_array()) else {
        return;
    };
    if arr.is_empty() {
        return;
    }
    summary.push_str(header);
    for value in arr {
        if let Some(value) = value.as_str() {
            summary.push_str("\n");
            if bullet_prefix {
                summary.push_str("- ");
            }
            summary.push_str(value);
        }
    }
}

fn build_cargo_test_summary(
    label: &str,
    failures_json: &Value,
    log_path: Option<&Path>,
    summary_line: Option<&str>,
) -> String {
    let mut summary = label.to_string();
    append_cargo_test_failure_section(
        &mut summary,
        failures_json,
        "failed_tests",
        "\nfailed_tests:",
        true,
    );
    append_cargo_test_failure_section(
        &mut summary,
        failures_json,
        "error_locations",
        "\nerror_locations:",
        true,
    );
    append_cargo_test_failure_section(
        &mut summary,
        failures_json,
        "failure_block",
        "\nfailure_block:",
        false,
    );
    if let Some(path) = log_path {
        summary.push_str(&format!("\noutput_log: {}", path.display()));
    }
    if let Some(line) = summary_line {
        summary.push_str(&format!("\nsummary: {line}"));
    }
    summary
}

fn handle_cargo_test_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let crate_name = action
        .get("crate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("cargo_test missing 'crate'"))?;
    let test_name = action.get("test").and_then(|v| v.as_str());
    let cached_failed = if test_name.is_none() {
        load_cached_failed_tests(workspace)
    } else {
        Vec::new()
    };
    let cmd = if let Some(test_name) = test_name {
        build_cargo_test_command(crate_name, Some(test_name))
    } else if let Some(retry_cmd) = build_cached_failed_tests_command(crate_name, &cached_failed) {
        retry_cmd
    } else {
        build_cargo_test_command(crate_name, None)
    };
    eprintln!("[{role}] step={} cargo_test cmd={}", step, cmd);
    let (spawn_ok, out) = exec_run_command(workspace, &cmd, crate::constants::workspace())?;
    let log_path = extract_output_log_path(&out);
    let summary_line = cargo_test_summary_line(log_path.as_deref(), &out);
    let label = cargo_test_label(summary_line.as_deref(), spawn_ok);
    eprintln!("[{role}] step={} {label} output_bytes={}", step, out.len());
    if !spawn_ok {
        log_error_event(
            role,
            "cargo_test",
            Some(step),
            &format!("cargo_test failed for crate {crate_name}"),
            Some(json!({
                "stage": "cargo_test",
                "crate": crate_name,
                "test": test_name,
                "cmd": cmd,
            })),
        );
    }
    let failures_json = parse_cargo_test_failures(&out);
    let _ = fs::write(
        workspace.join("cargo_test_failures.json"),
        serde_json::to_string_pretty(&failures_json).unwrap_or_else(|_| failures_json.to_string()),
    );
    let summary = build_cargo_test_summary(
        label,
        &failures_json,
        log_path.as_deref(),
        summary_line.as_deref(),
    );
    Ok((false, summary))
}

fn handle_cargo_fmt_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let fix = action.get("fix").and_then(|v| v.as_bool()).unwrap_or(false);
    let cmd = if fix {
        "cargo fmt"
    } else {
        "cargo fmt --check"
    };
    let timeout_secs = env::var("CANON_CARGO_FMT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5 * 60);
    eprintln!("[{role}] step={} cargo_fmt cmd={cmd}", step);
    let (ok, out) = exec_run_command_blocking_with_timeout(
        workspace,
        cmd,
        crate::constants::workspace(),
        timeout_secs,
    )?;
    let log_rel = write_state_log(workspace, "cargo_fmt", &out)?;
    let status = if ok {
        "cargo_fmt ok"
    } else {
        "cargo_fmt failed"
    };
    let diff_files = out
        .lines()
        .filter(|l| l.trim_start().starts_with("Diff in "))
        .count();
    let summary = if ok {
        if fix {
            "formatted".to_string()
        } else if diff_files == 0 {
            "no formatting diffs".to_string()
        } else {
            format!("formatting diffs found (files={diff_files})")
        }
    } else if diff_files > 0 {
        format!("formatting diffs found (files={diff_files})")
    } else {
        summarize_compiler_like_output(&out)
    };
    Ok((
        false,
        format!("{status}\nlog: {log_rel}\nsummary: {summary}"),
    ))
}

fn handle_cargo_clippy_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let crate_name = action
        .get("crate")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let cmd = if let Some(krate) = crate_name {
        format!(
            "env RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo clippy -p {krate} -- -D warnings"
        )
    } else {
        "env RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo clippy -- -D warnings".to_string()
    };
    let timeout_secs = env::var("CANON_CARGO_CLIPPY_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20 * 60);
    eprintln!("[{role}] step={} cargo_clippy cmd={cmd}", step);
    let (ok, out) = exec_run_command_blocking_with_timeout(
        workspace,
        &cmd,
        crate::constants::workspace(),
        timeout_secs,
    )?;
    let log_rel = write_state_log(workspace, "cargo_clippy", &out)?;
    let status = if ok {
        "cargo_clippy ok"
    } else {
        "cargo_clippy failed"
    };
    let summary = summarize_compiler_like_output(&out);
    Ok((
        false,
        format!("{status}\nlog: {log_rel}\nsummary: {summary}"),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanOp {
    CreateTask,
    UpdateTask,
    DeleteTask,
    AddEdge,
    RemoveEdge,
    SetPlanStatus,
    SetTaskStatus,
    ReplacePlan,
}

impl PlanOp {
    const VALID_OPS: &'static str =
        "create_task, update_task, delete_task, add_edge, remove_edge, set_plan_status, set_task_status, replace_plan";

    fn parse(op: &str) -> Result<Self> {
        match op {
            "create_task" => Ok(Self::CreateTask),
            "update_task" => Ok(Self::UpdateTask),
            "delete_task" => Ok(Self::DeleteTask),
            "add_edge" => Ok(Self::AddEdge),
            "remove_edge" => Ok(Self::RemoveEdge),
            "set_plan_status" => Ok(Self::SetPlanStatus),
            "set_task_status" => Ok(Self::SetTaskStatus),
            "replace_plan" => Ok(Self::ReplacePlan),
            _ => bail!("unknown plan op: {op} (valid ops: {})", Self::VALID_OPS),
        }
    }
}

fn task_status_value(task: &serde_json::Map<String, Value>) -> Option<&str> {
    task.get("status").and_then(|v| v.as_str()).map(str::trim)
}

fn reopened_task_needs_regression_linkage(
    existing: &serde_json::Map<String, Value>,
    updated: &serde_json::Map<String, Value>,
) -> bool {
    let was_done = matches!(task_status_value(existing), Some("done"));
    let next_status = updated
        .get("status")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .or_else(|| task_status_value(existing));
    let is_reopened = was_done && !matches!(next_status, Some("done"));
    if !is_reopened {
        return false;
    }
    let steps = updated
        .get("steps")
        .or_else(|| existing.get("steps"))
        .and_then(|v| v.as_array());
    let has_regression_linkage = steps
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
        .any(|s| s.contains("regression"));
    !has_regression_linkage
}

fn ensure_reopened_task_has_regression_linkage(
    existing: &serde_json::Map<String, Value>,
    updated: &serde_json::Map<String, Value>,
    task_id: &str,
) -> Result<()> {
    if reopened_task_needs_regression_linkage(existing, updated) {
        bail!(
            "reopened task {task_id} must include regression-test linkage in steps when status changes from done to a non-done state"
        );
    }
    Ok(())
}

fn validate_plan_action_shape(action: &Value, normalized_op: &str) -> Result<()> {
    match normalized_op {
        "create_task" => validate_plan_create_task_shape(action, normalized_op),
        "update_task" => validate_plan_update_task_shape(action, normalized_op),
        "delete_task" => validate_plan_delete_task_shape(action, normalized_op),
        "add_edge" | "remove_edge" => validate_plan_edge_shape(action, normalized_op),
        "set_plan_status" => validate_plan_set_plan_status_shape(action, normalized_op),
        "set_task_status" => validate_plan_set_task_status_shape(action, normalized_op),
        "replace_plan" => validate_plan_replace_plan_shape(action, normalized_op),
        "sorted_view" | "update" => Ok(()),
        _ => Ok(()),
    }
}

fn plan_action_has_field(action: &Value, field: &str) -> bool {
    action.get(field).is_some()
}

fn require_plan_action_field(action: &Value, normalized_op: &str, field: &str) -> Result<()> {
    if plan_action_has_field(action, field) {
        Ok(())
    } else {
        Err(anyhow!("plan {normalized_op} missing {field}"))
    }
}

fn reject_plan_action_field(
    action: &Value,
    normalized_op: &str,
    field: &str,
    why: &str,
) -> Result<()> {
    if plan_action_has_field(action, field) {
        Err(anyhow!(
            "plan {normalized_op} does not accept {field} ({why})"
        ))
    } else {
        Ok(())
    }
}

fn require_plan_action_fields(action: &Value, normalized_op: &str, fields: &[&str]) -> Result<()> {
    fields
        .iter()
        .try_for_each(|field| require_plan_action_field(action, normalized_op, field))
}

fn reject_plan_action_fields(
    action: &Value,
    normalized_op: &str,
    fields: &[(&str, &str)],
) -> Result<()> {
    for (field, why) in fields {
        reject_plan_action_field(action, normalized_op, field, why)?;
    }
    Ok(())
}

fn validate_plan_create_task_shape(action: &Value, normalized_op: &str) -> Result<()> {
    require_plan_action_field(action, normalized_op, "task")?;
    reject_plan_action_fields(
        action,
        normalized_op,
        &[
            ("status", "set status inside task object"),
            ("from", "edge fields are only for add_edge/remove_edge"),
            ("to", "edge fields are only for add_edge/remove_edge"),
            ("plan", "use replace_plan to write a full plan object"),
        ],
    )
}

fn validate_plan_update_task_shape(action: &Value, normalized_op: &str) -> Result<()> {
    require_plan_action_field(action, normalized_op, "task")?;
    reject_plan_action_fields(
        action,
        normalized_op,
        &[
            (
                "status",
                "set status inside task object or use set_task_status",
            ),
            ("from", "edge fields are only for add_edge/remove_edge"),
            ("to", "edge fields are only for add_edge/remove_edge"),
            ("plan", "use replace_plan to write a full plan object"),
        ],
    )
}

fn validate_plan_delete_task_shape(action: &Value, normalized_op: &str) -> Result<()> {
    require_plan_action_field(action, normalized_op, "task_id")?;
    reject_plan_action_fields(
        action,
        normalized_op,
        &[
            ("task", "delete_task targets by task_id only"),
            ("status", "status is not used by delete_task"),
            ("from", "edge fields are only for add_edge/remove_edge"),
            ("to", "edge fields are only for add_edge/remove_edge"),
            ("plan", "use replace_plan to write a full plan object"),
        ],
    )
}

fn validate_plan_edge_shape(action: &Value, normalized_op: &str) -> Result<()> {
    require_plan_action_fields(action, normalized_op, &["from", "to"])?;
    reject_plan_action_fields(
        action,
        normalized_op,
        &[
            ("task", "task object is not used for edge operations"),
            ("status", "status is not used for edge operations"),
            ("plan", "use replace_plan to write a full plan object"),
        ],
    )
}

fn validate_plan_set_plan_status_shape(action: &Value, normalized_op: &str) -> Result<()> {
    require_plan_action_field(action, normalized_op, "status")?;
    reject_plan_action_fields(
        action,
        normalized_op,
        &[
            ("task", "set_plan_status changes PLAN.status only"),
            ("from", "edge fields are only for add_edge/remove_edge"),
            ("to", "edge fields are only for add_edge/remove_edge"),
            ("plan", "use replace_plan to write a full plan object"),
        ],
    )
}

fn validate_plan_set_task_status_shape(action: &Value, normalized_op: &str) -> Result<()> {
    require_plan_action_fields(action, normalized_op, &["task_id", "status"])?;
    reject_plan_action_fields(
        action,
        normalized_op,
        &[
            ("task", "use update_task for full task updates"),
            ("from", "edge fields are only for add_edge/remove_edge"),
            ("to", "edge fields are only for add_edge/remove_edge"),
            ("plan", "use replace_plan to write a full plan object"),
        ],
    )
}

fn validate_plan_replace_plan_shape(action: &Value, normalized_op: &str) -> Result<()> {
    require_plan_action_field(action, normalized_op, "plan")?;
    reject_plan_action_fields(
        action,
        normalized_op,
        &[
            ("task", "replace_plan uses plan object"),
            ("status", "replace_plan uses plan object"),
            ("from", "replace_plan uses plan object"),
            ("to", "replace_plan uses plan object"),
        ],
    )
}

fn handle_plan_action(role: &str, workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let op_raw = extract_plan_op(action);
    preflight_plan_action(role, action, op_raw)?;
    if let Some(result) = handle_plan_fast_paths(workspace, action, op_raw)? {
        return Ok(result);
    }
    validate_plan_action_shape(action, op_raw)?;
    let op = PlanOp::parse(op_raw)?;
    let plan_path = workspace.join(MASTER_PLAN_FILE);
    let mut plan = load_or_init_plan(&plan_path)?;
    match op {
        PlanOp::ReplacePlan => {
            plan = build_replacement_plan(action)?;
        }
        _ => {
            let obj = plan
                .as_object_mut()
                .ok_or_else(|| anyhow!("PLAN.json must be a JSON object"))?;
            if let Some(result) = dispatch_plan_op(op, obj, action)? {
                return Ok(result);
            }
        }
    }

    sync_plan_ready_window(&mut plan)?;

    persist_plan_action_update(role, action, op_raw, &plan_path, &plan)?;

    // Planner cycle terminator: a plan op that lands a task in `ready` state is
    // sufficient to end the planner's turn. Returning done=true here exits
    // run_agent the same way a `message` action would, eliminating redundant
    // handoff steps.
    //
    // Other roles (verifier, solo) are not affected — their plan actions continue
    // to return done=false so their own cycle termination logic is unchanged.
    // Planner cycle terminator: only fire on set_task_status→ready, which is the
    // atomic "I am done mutating, mark this task executable" primitive.  Other ops
    // (create_task, update_task, replace_plan) are mid-sequence and may be followed
    // by edge additions or further mutations — terminating on those would cut the
    // cycle short.
    if role.eq_ignore_ascii_case("planner") && plan_op_is_terminal_ready(op_raw, action) {
        let task_id = action
            .get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("(see plan)");
        eprintln!(
            "[plan] planner cycle complete via set_task_status→ready `{task_id}`; \
             no handoff message required"
        );
        return Ok((
            true,
            format!(
                "plan ok — ready task `{task_id}` dispatched\n\
                 plan_path: {}",
                plan_path.display()
            ),
        ));
    }

    // Executor completion terminal: executor marks its task done.
    // so it can schedule the next task.  Return done=true to end the executor's turn
    // without requiring a separate `message` action.
    if role.starts_with("executor")
        && op_raw == "set_task_status"
        && action
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("done") || s.eq_ignore_ascii_case("complete"))
            .unwrap_or(false)
    {
        let task_id = action
            .get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("(see plan)");
        eprintln!(
            "[plan] executor marked task `{task_id}` done; \
             no handoff message required"
        );
        return Ok((
            true,
            format!(
                "plan ok — task `{task_id}` marked done; planner scheduled\n\
                 plan_path: {}",
                plan_path.display()
            ),
        ));
    }

    Ok((
        false,
        format!("plan ok\nplan_path: {}", plan_path.display()),
    ))
}

fn sync_plan_ready_window(plan: &mut Value) -> Result<()> {
    let obj = plan
        .as_object_mut()
        .ok_or_else(|| anyhow!("PLAN.json must be a JSON object"))?;
    let tasks = obj
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;

    let ready_window = tasks
        .iter()
        .filter_map(|task| {
            let is_ready = task
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s.eq_ignore_ascii_case("ready"))
                .unwrap_or(false);
            if !is_ready {
                return None;
            }
            task.get("id")
                .and_then(|v| v.as_str())
                .map(|id| Value::String(id.to_string()))
        })
        .collect();

    obj.insert("ready_window".to_string(), Value::Array(ready_window));
    Ok(())
}

fn dispatch_plan_op(
    op: PlanOp,
    obj: &mut serde_json::Map<String, Value>,
    action: &Value,
) -> Result<Option<(bool, String)>> {
    match op {
        PlanOp::CreateTask => {
            handle_plan_create_task(obj, action)?;
        }
        PlanOp::UpdateTask => {
            handle_plan_update_task(obj, action)?;
        }
        PlanOp::DeleteTask => {
            handle_plan_delete_task(obj, action)?;
        }
        PlanOp::AddEdge => {
            if let Some(result) = handle_plan_add_edge(obj, action)? {
                return Ok(Some(result));
            }
        }
        PlanOp::RemoveEdge => {
            handle_plan_remove_edge(obj, action)?;
        }
        PlanOp::SetPlanStatus => {
            handle_plan_set_plan_status(obj, action)?;
        }
        PlanOp::SetTaskStatus => {
            handle_plan_set_task_status(obj, action)?;
        }
        PlanOp::ReplacePlan => {
            unreachable!("replace_plan is handled before object mutation dispatch")
        }
    }
    Ok(None)
}

fn persist_plan_action_update(
    role: &str,
    action: &Value,
    op_raw: &str,
    plan_path: &Path,
    plan: &Value,
) -> Result<()> {
    write_projection_with_workspace_effects(
        std::path::Path::new(crate::constants::workspace()),
        plan_path,
        MASTER_PLAN_FILE,
        &format!("plan_update:{op_raw}"),
        &serde_json::to_string_pretty(plan)?,
    )?;
    // Emit control-plane log for plan mutation
    if let Ok(_paths) =
        crate::logging::append_action_log_record(&crate::logging::compact_log_record(
            "control",
            "plan_update",
            Some(role),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(true),
            Some("PLAN.json updated via plan action".to_string()),
            action
                .get("rationale")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            None,
            Some(json!({"op": op_raw, "path": MASTER_PLAN_FILE})),
        ))
    {}
    let _ = plan_op_produced_ready_task(op_raw, action, plan);
    Ok(())
}

fn persist_plan_bundle_projection(
    workspace: &Path,
    action: &Value,
    op_raw: &str,
    plan_path: &Path,
    plan: &Value,
    success_message: &str,
) -> Result<()> {
    write_projection_with_workspace_effects(
        workspace,
        plan_path,
        MASTER_PLAN_FILE,
        &format!("plan_update:{op_raw}"),
        &serde_json::to_string_pretty(plan)?,
    )?;
    let _ = crate::logging::append_action_log_record(&crate::logging::compact_log_record(
        "control",
        "plan_update",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(true),
        Some(success_message.to_string()),
        action
            .get("rationale")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        None,
        Some(json!({"op": op_raw, "path": MASTER_PLAN_FILE})),
    ));
    let _ = plan_op_produced_ready_task(op_raw, action, plan);
    Ok(())
}

/// Return true when the plan mutation just written resulted in at least one task
/// having `status = "ready"`.
fn plan_op_produced_ready_task(op_raw: &str, action: &Value, plan: &Value) -> bool {
    match op_raw {
        "set_task_status" | "update_task" => action
            .get("status")
            .or_else(|| action.get("task").and_then(|t| t.get("status")))
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("ready"))
            .unwrap_or(false),
        "create_task" => action
            .get("task")
            .and_then(|t| t.get("status"))
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("ready"))
            .unwrap_or(false),
        // replace_plan — scan the written plan for any ready task.
        "replace_plan" => plan
            .get("tasks")
            .and_then(|v| v.as_array())
            .map(|tasks| {
                tasks.iter().any(|t| {
                    t.get("status")
                        .and_then(|v| v.as_str())
                        .map(|s| s.eq_ignore_ascii_case("ready"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false),
        _ => false,
    }
}

/// Return true when this plan op is the unambiguous terminal "mark ready" primitive
/// that should end the planner's cycle.  Only `set_task_status` qualifies — it is
/// the atomic "I am done with mutations, flip this task to ready" operation.
///
/// `create_task`, `update_task`, and `replace_plan` are mid-sequence ops: the planner
/// typically follows them with edge additions, further mutations, or a message, so
/// terminating early would cut the cycle before that work is done.
fn plan_op_is_terminal_ready(op_raw: &str, action: &Value) -> bool {
    op_raw == "set_task_status"
        && action
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("ready"))
            .unwrap_or(false)
}

fn build_replacement_plan(action: &Value) -> Result<Value> {
    let mut next_plan = action
        .get("plan")
        .cloned()
        .ok_or_else(|| anyhow!("plan replace_plan missing plan object"))?;
    normalize_plan_object(&mut next_plan)?;
    let tasks = next_plan
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    let edges = next_plan
        .get("dag")
        .and_then(|v| v.get("edges"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
    ensure_dag(tasks, edges)?;
    Ok(next_plan)
}

fn handle_plan_fast_paths(
    workspace: &Path,
    action: &Value,
    op_raw: &str,
) -> Result<Option<(bool, String)>> {
    if op_raw == "sorted_view" {
        return Ok(Some(handle_plan_sorted_view_action(workspace)?));
    }
    if op_raw == "update" {
        if action.get("updates").is_none() && action.get("plan").is_some() {
            return Ok(Some(handle_plan_replace_bundle(workspace, action)?));
        }
        return Ok(Some(handle_plan_update_bundle(workspace, action)?));
    }
    Ok(None)
}

fn handle_plan_add_edge(
    obj: &mut serde_json::Map<String, Value>,
    action: &Value,
) -> Result<Option<(bool, String)>> {
    let tasks = get_tasks_array(obj)?;
    let ids = collect_task_ids(tasks);

    let (from, to) = extract_edge_endpoints(action)?;
    validate_edge_ids(&ids, from, to)?;

    let edges = get_edges_array_mut(obj)?;
    if edge_exists(edges, from, to) {
        return Ok(Some((false, "plan edge already exists".to_string())));
    }

    push_edge(edges, from, to);
    let edges_snapshot = edges.clone();

    let tasks = get_tasks_array(obj)?;
    ensure_dag(tasks, &edges_snapshot)?;
    Ok(None)
}

fn get_tasks_array(obj: &serde_json::Map<String, Value>) -> Result<&Vec<Value>> {
    obj.get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))
}

fn extract_edge_endpoints(action: &Value) -> Result<(&str, &str)> {
    let from = action
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plan add_edge missing from"))?;
    let to = action
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plan add_edge missing to"))?;
    Ok((from, to))
}

fn validate_edge_ids(ids: &std::collections::BTreeSet<String>, from: &str, to: &str) -> Result<()> {
    if !ids.contains(from) || !ids.contains(to) {
        bail!("plan edge refers to unknown task id");
    }
    Ok(())
}

fn get_edges_array_mut(obj: &mut serde_json::Map<String, Value>) -> Result<&mut Vec<Value>> {
    obj.get_mut("dag")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing dag object"))?
        .get_mut("edges")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))
}

fn edge_exists(edges: &Vec<Value>, from: &str, to: &str) -> bool {
    edges.iter().any(|e| {
        e.get("from").and_then(|v| v.as_str()) == Some(from)
            && e.get("to").and_then(|v| v.as_str()) == Some(to)
    })
}

fn push_edge(edges: &mut Vec<Value>, from: &str, to: &str) {
    let mut edge = serde_json::Map::new();
    edge.insert("from".to_string(), Value::String(from.to_string()));
    edge.insert("to".to_string(), Value::String(to.to_string()));
    edges.push(Value::Object(edge));
}

fn handle_plan_remove_edge(obj: &mut serde_json::Map<String, Value>, action: &Value) -> Result<()> {
    let dag = obj
        .get_mut("dag")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing dag object"))?;
    let edges = dag
        .get_mut("edges")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
    let from = action
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plan remove_edge missing from"))?;
    let to = action
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plan remove_edge missing to"))?;
    edges.retain(|e| {
        let e_from = e.get("from").and_then(|v| v.as_str());
        let e_to = e.get("to").and_then(|v| v.as_str());
        !(e_from == Some(from) && e_to == Some(to))
    });
    Ok(())
}

fn handle_plan_set_plan_status(
    obj: &mut serde_json::Map<String, Value>,
    action: &Value,
) -> Result<()> {
    let status = action
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plan set_plan_status missing status"))?;
    if status == "done" {
        let tasks = obj
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
        let any_incomplete = tasks.iter().any(|t| {
            t.get("status")
                .and_then(|v| v.as_str())
                .map(|s| s.trim() != "done")
                .unwrap_or(true)
        });
        if any_incomplete {
            bail!("plan status cannot be set to done while tasks remain incomplete");
        }
    }
    obj.insert("status".to_string(), Value::String(status.to_string()));
    Ok(())
}

fn extract_plan_op(action: &Value) -> &str {
    let op = action
        .get("op")
        .and_then(|v| v.as_str())
        .or_else(|| action.get("operation").and_then(|v| v.as_str()))
        .unwrap_or("update");
    match op {
        "create_edge" => "add_edge",
        "delete_edge" => "remove_edge",
        _ => op,
    }
}

fn preflight_plan_action(role: &str, action: &Value, op_raw: &str) -> Result<()> {
    // Executors may only use `set_task_status → done/complete` to close the task
    // they just finished.  Every other plan mutation remains planner-only.
    if role.starts_with("executor") && op_raw != "sorted_view" {
        let is_marking_done = op_raw == "set_task_status"
            && action
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s.eq_ignore_ascii_case("done") || s.eq_ignore_ascii_case("complete"))
                .unwrap_or(false);
        if !is_marking_done {
            bail!(
                "plan action is not allowed for executor roles \
                 (only `set_task_status → done` is permitted — use it after tests pass)"
            );
        }
    }
    if op_raw != "sorted_view" {
        capture_plan_schema(action);
    }
    validate_planner_diagnostics(role, action)?;
    if let Some(path) = action.get("path").and_then(|v| v.as_str()) {
        if path != MASTER_PLAN_FILE {
            bail!("plan path must be {MASTER_PLAN_FILE}, got {path}");
        }
    }
    Ok(())
}

fn validate_planner_diagnostics(role: &str, action: &Value) -> Result<()> {
    if !matches!(role, "planner" | "mini_planner") {
        return Ok(());
    }
    let rationale = action
        .get("rationale")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let observation = action
        .get("observation")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let combined = format!("{observation}\n{rationale}").to_ascii_lowercase();
    let references_diagnostics = combined.contains("diagnostic")
        || combined.contains("stale")
        || combined.contains("violation");
    let has_source_validation = combined.contains("read_file")
        || combined.contains("source")
        || combined.contains("verified")
        || combined.contains("current-cycle")
        || combined.contains("rg ")
        || combined.contains("run_command");
    if references_diagnostics && !has_source_validation {
        bail!("planner plan actions that rely on diagnostics must cite current source validation in observation/rationale (for example read_file, run_command, or verified source evidence)");
    }
    Ok(())
}

fn handle_plan_create_task(obj: &mut serde_json::Map<String, Value>, action: &Value) -> Result<()> {
    let tasks = obj
        .get_mut("tasks")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    let task = action
        .get("task")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("plan create_task missing task object"))?;
    let id = task
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plan task missing id"))?;
    if tasks
        .iter()
        .any(|t| t.get("id").and_then(|v| v.as_str()) == Some(id))
    {
        bail!("plan task already exists: {id}");
    }
    // Copy all fields from the task object so nothing is silently dropped
    // (e.g. objective_id, issue_refs, priority, steps, title).
    // `id` and `status` are always written from their canonical sources so
    // they cannot be omitted even if absent in the incoming task object.
    let mut new_task = task.clone();
    new_task.insert("id".to_string(), Value::String(id.to_string()));
    let status = task
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("todo");
    new_task.insert("status".to_string(), Value::String(status.to_string()));
    tasks.push(Value::Object(new_task));
    Ok(())
}

fn handle_plan_update_task(obj: &mut serde_json::Map<String, Value>, action: &Value) -> Result<()> {
    let tasks = obj
        .get_mut("tasks")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    let task = action
        .get("task")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("plan update_task missing task object. Required schema: {{\"op\":\"update_task\",\"task\":{{\"id\":\"<id>\",\"status\":\"<status>\"}}}}"))?;
    let id = task
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plan task missing id"))?;
    let Some(existing) = tasks
        .iter_mut()
        .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(id))
        .and_then(|t| t.as_object_mut())
    else {
        bail!("plan task not found: {id}");
    };
    ensure_reopened_task_has_regression_linkage(existing, task, id)?;
    for (key, value) in task {
        if key != "id" {
            existing.insert(key.to_string(), value.clone());
        }
    }
    // Track the active task for provenance threading.
    let new_status = existing
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if new_status.eq_ignore_ascii_case("in_progress") {
        crate::constants::set_active_task_id(id);
    } else if crate::issues::is_done_like_status(new_status) {
        crate::constants::set_active_task_id("");
    }
    Ok(())
}

fn handle_plan_set_task_status(
    obj: &mut serde_json::Map<String, Value>,
    action: &Value,
) -> Result<()> {
    let task_id = action
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plan set_task_status missing task_id"))?;
    let status = action
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plan set_task_status missing status"))?;
    let tasks = obj
        .get_mut("tasks")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    let Some(existing) = tasks
        .iter_mut()
        .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(task_id))
        .and_then(|t| t.as_object_mut())
    else {
        bail!("plan task not found: {task_id}");
    };
    let mut updated = serde_json::Map::new();
    updated.insert("status".to_string(), Value::String(status.to_string()));
    ensure_reopened_task_has_regression_linkage(existing, &updated, task_id)?;
    existing.insert("status".to_string(), Value::String(status.to_string()));
    // Track the active task for provenance threading.
    if status.eq_ignore_ascii_case("in_progress") {
        crate::constants::set_active_task_id(task_id);
    } else if crate::issues::is_done_like_status(status) {
        crate::constants::set_active_task_id("");
    }
    Ok(())
}

fn handle_plan_delete_task(obj: &mut serde_json::Map<String, Value>, action: &Value) -> Result<()> {
    let tasks = obj
        .get_mut("tasks")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    let task_id = action
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("plan delete_task missing task_id"))?;
    tasks.retain(|t| t.get("id").and_then(|v| v.as_str()) != Some(task_id));

    let dag = obj
        .get_mut("dag")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing dag object"))?;
    let edges = dag
        .get_mut("edges")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
    edges.retain(|e| {
        let from = e.get("from").and_then(|v| v.as_str());
        let to = e.get("to").and_then(|v| v.as_str());
        from != Some(task_id) && to != Some(task_id)
    });
    Ok(())
}

fn capture_plan_schema(action: &Value) {
    let path =
        std::path::Path::new(crate::constants::agent_state_dir()).join("plan_action_schemas.jsonl");
    let record = json!({
        "ts_ms": now_ms(),
        "action": action,
    });
    if let Ok(line) = serde_json::to_string(&record) {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{}", line));
    }
}

fn apply_plan_bundle_status(
    obj: &mut serde_json::Map<String, Value>,
    updates: &serde_json::Map<String, Value>,
) {
    if let Some(status) = updates.get("status").and_then(|v| v.as_str()) {
        obj.insert("status".to_string(), Value::String(status.to_string()));
    }
}

fn apply_plan_bundle_task_patch(
    existing: &mut serde_json::Map<String, Value>,
    task_obj: &serde_json::Map<String, Value>,
    id: &str,
) -> Result<()> {
    ensure_reopened_task_has_regression_linkage(existing, task_obj, id)?;
    for (key, value) in task_obj {
        if key != "id" {
            existing.insert(key.to_string(), value.clone());
        }
    }
    Ok(())
}

fn apply_plan_bundle_task_updates(
    obj: &mut serde_json::Map<String, Value>,
    updates: &serde_json::Map<String, Value>,
) -> Result<()> {
    let Some(tasks) = updates.get("tasks").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    let tasks_obj = obj
        .get_mut("tasks")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    for task in tasks {
        let task_obj = task
            .as_object()
            .ok_or_else(|| anyhow!("plan update tasks must be objects"))?;
        let id = task_obj
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("plan update task missing id"))?;
        let Some(existing) = tasks_obj
            .iter_mut()
            .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(id))
            .and_then(|t| t.as_object_mut())
        else {
            bail!("plan task not found: {id}");
        };
        apply_plan_bundle_task_patch(existing, task_obj, id)?;
    }
    Ok(())
}

fn plan_dag_edges_mut(obj: &mut serde_json::Map<String, Value>) -> Result<&mut Vec<Value>> {
    obj.get_mut("dag")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing dag object"))?
        .get_mut("edges")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))
}

fn apply_plan_bundle_remove_edges(
    obj: &mut serde_json::Map<String, Value>,
    updates: &serde_json::Map<String, Value>,
) -> Result<()> {
    let Some(edges) = updates.get("remove_edges").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    let edges_obj = plan_dag_edges_mut(obj)?;
    for edge in edges {
        let from = edge.get("from").and_then(|v| v.as_str());
        let to = edge.get("to").and_then(|v| v.as_str());
        if let (Some(from), Some(to)) = (from, to) {
            edges_obj.retain(|e| {
                let e_from = e.get("from").and_then(|v| v.as_str());
                let e_to = e.get("to").and_then(|v| v.as_str());
                !(e_from == Some(from) && e_to == Some(to))
            });
        }
    }
    Ok(())
}

fn apply_plan_bundle_add_edges(
    obj: &mut serde_json::Map<String, Value>,
    updates: &serde_json::Map<String, Value>,
) -> Result<()> {
    let Some(edges) = updates.get("add_edges").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    let ids = {
        let tasks = obj
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
        collect_task_ids(tasks)
    };
    let edges_obj = plan_dag_edges_mut(obj)?;
    for edge in edges {
        let from = edge
            .get("from")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("plan add_edge missing from"))?;
        let to = edge
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("plan add_edge missing to"))?;
        if !ids.contains(from) || !ids.contains(to) {
            bail!("plan edge refers to unknown task id");
        }
        if edges_obj.iter().any(|e| {
            e.get("from").and_then(|v| v.as_str()) == Some(from)
                && e.get("to").and_then(|v| v.as_str()) == Some(to)
        }) {
            continue;
        }
        let mut edge_obj = serde_json::Map::new();
        edge_obj.insert("from".to_string(), Value::String(from.to_string()));
        edge_obj.insert("to".to_string(), Value::String(to.to_string()));
        edges_obj.push(Value::Object(edge_obj));
    }
    let edges_snapshot = edges_obj.clone();
    let tasks = obj
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    ensure_dag(tasks, &edges_snapshot)?;
    Ok(())
}

fn handle_plan_update_bundle(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let updates = action
        .get("updates")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("plan update missing updates object"))?;
    let plan_path = workspace.join(MASTER_PLAN_FILE);
    let mut plan = load_or_init_plan(&plan_path)?;
    let obj = plan
        .as_object_mut()
        .ok_or_else(|| anyhow!("PLAN.json must be a JSON object"))?;

    apply_plan_bundle_status(obj, updates);
    apply_plan_bundle_task_updates(obj, updates)?;
    apply_plan_bundle_remove_edges(obj, updates)?;
    apply_plan_bundle_add_edges(obj, updates)?;

    persist_plan_bundle_projection(
        workspace,
        action,
        "update_bundle",
        &plan_path,
        &plan,
        "PLAN.json updated via plan update bundle",
    )?;
    Ok((
        false,
        format!("plan ok\nplan_path: {}", plan_path.display()),
    ))
}

fn handle_plan_replace_bundle(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let plan_path = workspace.join(MASTER_PLAN_FILE);
    let mut next_plan = action
        .get("plan")
        .cloned()
        .ok_or_else(|| anyhow!("plan replace_plan missing plan object"))?;
    normalize_plan_object(&mut next_plan)?;
    let tasks = next_plan
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    let edges = next_plan
        .get("dag")
        .and_then(|v| v.get("edges"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
    ensure_dag(tasks, edges)?;
    persist_plan_bundle_projection(
        workspace,
        action,
        "replace_bundle",
        &plan_path,
        &next_plan,
        "PLAN.json replaced via plan action",
    )?;
    Ok((
        false,
        format!("plan ok\nplan_path: {}", plan_path.display()),
    ))
}

fn load_or_init_plan(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut plan = if raw.trim().is_empty() {
        json!({
            "version": 2,
            "status": "in_progress",
            "tasks": [],
            "dag": { "edges": [] }
        })
    } else {
        serde_json::from_str(&raw)?
    };
    normalize_plan_object(&mut plan)?;
    Ok(plan)
}

fn normalize_plan_object(plan: &mut Value) -> Result<()> {
    let obj = plan
        .as_object_mut()
        .ok_or_else(|| anyhow!("PLAN.json must be a JSON object"))?;
    let version_val = obj.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
    if version_val < 2 {
        obj.insert("version".to_string(), Value::Number(2.into()));
    }
    if obj.get("status").and_then(|v| v.as_str()).is_none() {
        obj.insert(
            "status".to_string(),
            Value::String("in_progress".to_string()),
        );
    }
    if obj.get("tasks").and_then(|v| v.as_array()).is_none() {
        obj.insert("tasks".to_string(), Value::Array(Vec::new()));
    }
    let dag = obj.entry("dag".to_string()).or_insert_with(|| json!({}));
    if dag.get("edges").and_then(|v| v.as_array()).is_none() {
        dag.as_object_mut()
            .ok_or_else(|| anyhow!("PLAN.json dag must be object"))?
            .insert("edges".to_string(), Value::Array(Vec::new()));
    }
    Ok(())
}

fn collect_task_ids(tasks: &[Value]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for task in tasks {
        if let Some(id) = task.get("id").and_then(|v| v.as_str()) {
            ids.insert(id.to_string());
        }
    }
    ids
}

fn ensure_dag(tasks: &[Value], edges: &[Value]) -> Result<()> {
    let ids = collect_task_ids(tasks);
    let mut adj: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for id in &ids {
        adj.insert(id.clone(), Vec::new());
    }
    for edge in edges {
        let from = edge.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let to = edge.get("to").and_then(|v| v.as_str()).unwrap_or("");
        if from.is_empty() || to.is_empty() {
            bail!("plan edge missing from/to");
        }
        if !ids.contains(from) || !ids.contains(to) {
            bail!("plan edge refers to unknown task id");
        }
        adj.entry(from.to_string())
            .or_default()
            .push(to.to_string());
    }
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();

    for id in ids {
        let mut stack = vec![(id.as_str(), false)];
        while let Some((node, exiting)) = stack.pop() {
            if exiting {
                visiting.remove(node);
                visited.insert(node.to_string());
                continue;
            }
            if visited.contains(node) {
                continue;
            }
            if visiting.contains(node) {
                bail!("plan DAG cycle detected at {node}");
            }
            visiting.insert(node.to_string());
            stack.push((node, true));
            if let Some(nexts) = adj.get(node) {
                for next in nexts.iter().rev() {
                    stack.push((next.as_str(), false));
                }
            }
        }
    }
    Ok(())
}

fn shell_tokens(cmd: &str) -> Vec<&str> {
    cmd.split(|c: char| c.is_whitespace() || matches!(c, '|' | '&' | ';' | '(' | ')' | '<' | '>'))
        .filter(|part| !part.is_empty())
        .collect()
}

fn contains_token_pair(cmd: &str, first: &str, second: &str) -> bool {
    let tokens = shell_tokens(cmd);
    tokens
        .windows(2)
        .any(|window| window[0] == first && window[1] == second)
}

fn looks_like_cargo_test(cmd: &str) -> bool {
    contains_token_pair(cmd, "cargo", "test")
}

fn starts_direct_debug_binary(cmd: &str) -> bool {
    let first = shell_tokens(cmd).into_iter().next().unwrap_or("");
    first.starts_with("./target/debug/") || first.contains("/target/debug/")
}

fn looks_like_long_running_command(cmd: &str) -> bool {
    contains_token_pair(cmd, "cargo", "run")
        || contains_token_pair(cmd, "cargo", "watch")
        || starts_direct_debug_binary(cmd)
        || cmd.contains(" --tlog ")
        || cmd.contains("| tee")
}

enum RunCommandKind {
    CargoTest,
    LongRunning,
    Blocking,
}

fn prepare_exec_run_command(
    workspace: &Path,
    cmd: &str,
    cwd: &str,
) -> Result<(PathBuf, RunCommandKind)> {
    let cwd_path = PathBuf::from(cwd);
    if !cwd_path.is_absolute() {
        bail!("run_command cwd must be absolute: {cwd}");
    }
    if !cwd_path.starts_with(workspace) && !cwd_path.starts_with("/tmp") {
        bail!("run_command cwd escapes workspace: {cwd}");
    }
    ensure_safe_command(cmd)?;
    let kind = if looks_like_cargo_test(cmd) {
        RunCommandKind::CargoTest
    } else if looks_like_long_running_command(cmd) {
        RunCommandKind::LongRunning
    } else {
        RunCommandKind::Blocking
    };
    Ok((cwd_path, kind))
}

fn exec_run_command(workspace: &Path, cmd: &str, cwd: &str) -> Result<(bool, String)> {
    let (cwd_path, kind) = prepare_exec_run_command(workspace, cmd, cwd)?;
    // Hybrid execution model:
    // - long-running commands → spawn (non-blocking)
    // - short commands → capture output (blocking)

    match kind {
        RunCommandKind::CargoTest => exec_run_command_cargo_test(cmd, &cwd_path),
        RunCommandKind::LongRunning => exec_run_command_spawn(cmd, &cwd_path),
        RunCommandKind::Blocking => exec_run_command_capture(cmd, &cwd_path),
    }
}

fn exec_run_command_spawn(cmd: &str, cwd_path: &Path) -> Result<(bool, String)> {
    let child = Command::new("/bin/bash")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| ctx_spawn(cmd))?;

    Ok((true, format!("spawned pid={}", child.id())))
}

fn exec_run_command_capture(cmd: &str, cwd_path: &Path) -> Result<(bool, String)> {
    let output = Command::new("/bin/bash")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd_path)
        .output()
        .with_context(|| ctx_spawn(cmd))?;

    let mut combined = combine_command_output(&output, cmd);
    append_trace_probe_info(&mut combined, cmd);

    Ok((output.status.success(), combined))
}

fn combine_command_output(output: &std::process::Output, cmd: &str) -> String {
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();

    if !output.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    if combined.trim().is_empty() && !output.status.success() {
        if cmd.contains("rg ") || cmd.contains("grep ") {
            combined = format!("no matches (exit={})", output.status.code().unwrap_or(-1));
            if cmd.contains("/tmp/runtime.trace") {
                combined.push_str("\ntrace probe returned no matches; file may be stale, missing, or the pattern may not be present yet");
            }
        }
    }

    combined
}

fn append_trace_probe_info(combined: &mut String, cmd: &str) {
    if cmd.contains("/tmp/runtime.trace") && (cmd.contains("rg ") || cmd.contains("grep ")) {
        let trace = PathBuf::from("/tmp/runtime.trace");
        match std::fs::metadata(&trace) {
            Ok(meta) => {
                combined.push_str(&format!(
                    "\ntrace_path=/tmp/runtime.trace trace_size={}B",
                    meta.len()
                ));
            }
            Err(_) => {
                combined.push_str("\ntrace_path=/tmp/runtime.trace trace_missing=true");
            }
        }
    }
}

fn exec_run_command_cargo_test(cmd: &str, cwd_path: &Path) -> Result<(bool, String)> {
    let timeout_secs = env::var("CANON_CARGO_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(8 * 60);
    let wrapped_cmd = format!("timeout -s TERM {}s {}", timeout_secs, cmd);
    let output = Command::new("/bin/bash")
        .arg("-c")
        .arg(&wrapped_cmd)
        .current_dir(cwd_path)
        .output()
        .with_context(|| ctx_spawn(&wrapped_cmd))?;
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis();
    let log_path = env::temp_dir().join(format!("canon-mini-agent-{pid}-{ts}.log"));
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    fs::write(&log_path, &combined)
        .with_context(|| format!("failed to write cargo_test log {}", log_path.display()))?;
    let summary_line = combined
        .lines()
        .rev()
        .find_map(|line| {
            line.find("test result:")
                .map(|idx| line[idx..].trim().to_string())
        })
        .unwrap_or_else(|| "(no test result yet)".to_string());
    let summary = format!(
        "output_log: {}\nsummary: {}",
        log_path.display(),
        summary_line
    );
    Ok((output.status.success(), summary))
}

fn exec_run_command_blocking_with_timeout(
    workspace: &Path,
    cmd: &str,
    cwd: &str,
    timeout_secs: u64,
) -> Result<(bool, String)> {
    let cwd_path = PathBuf::from(cwd);
    if !cwd_path.is_absolute() {
        bail!("run_command cwd must be absolute: {cwd}");
    }
    if !cwd_path.starts_with(workspace) && !cwd_path.starts_with("/tmp") {
        bail!("run_command cwd escapes workspace: {cwd}");
    }
    ensure_safe_command(cmd)?;
    let wrapped_cmd = format!("timeout -s TERM {}s {}", timeout_secs, cmd);
    let output = Command::new("/bin/bash")
        .arg("-c")
        .arg(&wrapped_cmd)
        .current_dir(&cwd_path)
        .output()
        .with_context(|| ctx_spawn(&wrapped_cmd))?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok((output.status.success(), combined))
}

fn ctx_read(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

fn ctx_spawn(cmd: &str) -> String {
    format!("failed to spawn: {cmd}")
}

fn exec_graph_command(workspace: &Path, cmd: &str) -> Result<(bool, String)> {
    let timeout_secs = env::var("CANON_GRAPH_CMD_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5 * 60);
    exec_run_command_blocking_with_timeout(
        workspace,
        cmd,
        crate::constants::workspace(),
        timeout_secs,
    )
}

fn exec_python(workspace: &Path, code: &str, cwd: &str) -> Result<(bool, String)> {
    let cwd_path = PathBuf::from(cwd);
    if !cwd_path.is_absolute() {
        bail!("python cwd must be absolute: {cwd}");
    }
    if !cwd_path.starts_with(workspace) && !cwd_path.starts_with("/tmp") {
        bail!("python cwd escapes workspace: {cwd}");
    }
    let mut child = Command::new("python3")
        .arg("-")
        .current_dir(&cwd_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn python3 in {}", cwd_path.display()))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(code.as_bytes())
            .context("failed writing python stdin")?;
    }
    let output = child
        .wait_with_output()
        .context("failed waiting for python3")?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok((output.status.success(), combined))
}

fn ensure_safe_command(cmd: &str) -> Result<()> {
    const BLOCKED: &[&str] = &[
        "rm -rf",
        "git reset --hard",
        "git clean -f",
        "dd if=",
        "mkfs",
        "shred",
    ];
    for needle in BLOCKED {
        if cmd.contains(needle) {
            bail!("blocked command: {cmd}");
        }
    }
    Ok(())
}

fn safe_join(workspace: &Path, relative: &str) -> Result<PathBuf> {
    let p = Path::new(relative);
    if p.is_absolute() {
        if p.starts_with(workspace) {
            return Ok(p.to_path_buf());
        }
        if p.starts_with("/tmp") {
            return Ok(p.to_path_buf());
        }
        bail!("absolute paths not allowed: {relative}");
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("path traversal not allowed: {relative}");
    }
    Ok(workspace.join(p))
}

fn execution_reports_dir(workspace: &Path) -> PathBuf {
    workspace
        .join("state")
        .join("reports")
        .join("execution_path")
}

fn execution_plan_latest_path(workspace: &Path, crate_name: &str) -> PathBuf {
    execution_reports_dir(workspace).join(format!("{crate_name}.latest.json"))
}

fn execution_plan_history_path(workspace: &Path, crate_name: &str) -> PathBuf {
    execution_reports_dir(workspace).join(format!("{crate_name}.jsonl"))
}

fn execution_learning_path(workspace: &Path) -> PathBuf {
    workspace
        .join("state")
        .join("reports")
        .join("execution_learning.jsonl")
}

fn persist_execution_path_plan(
    workspace: &Path,
    crate_name: &str,
    plan: &crate::semantic::ExecutionPathPlan,
) -> Result<()> {
    let out_dir = execution_reports_dir(workspace);
    fs::create_dir_all(&out_dir).with_context(|| format!("create dir {}", out_dir.display()))?;
    let latest_path = execution_plan_latest_path(workspace, crate_name);
    fs::write(&latest_path, serde_json::to_vec_pretty(plan)?)
        .with_context(|| format!("write {}", latest_path.display()))?;

    let history_path = execution_plan_history_path(workspace, crate_name);
    let mut history = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_path)
        .with_context(|| format!("open {}", history_path.display()))?;
    serde_json::to_writer(&mut history, plan)
        .with_context(|| format!("write {}", history_path.display()))?;
    history
        .write_all(b"\n")
        .with_context(|| format!("newline {}", history_path.display()))?;
    Ok(())
}

fn execution_plan_rebound_path(workspace: &Path, crate_name: &str) -> PathBuf {
    execution_reports_dir(workspace).join(format!("{crate_name}.rebound.json"))
}

fn persist_rebound_execution_plan(
    workspace: &Path,
    crate_name: &str,
    plan: &crate::semantic::ExecutionPathPlan,
) -> Result<()> {
    let out_path = execution_plan_rebound_path(workspace, crate_name);
    fs::write(&out_path, serde_json::to_vec_pretty(plan)?)
        .with_context(|| format!("write {}", out_path.display()))
}

fn load_execution_plan(
    workspace: &Path,
    crate_name: &str,
) -> Option<crate::semantic::ExecutionPathPlan> {
    let path = execution_plan_latest_path(workspace, crate_name);
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

#[derive(Default)]
struct LearningBiasStats {
    success_by_symbol: std::collections::HashMap<String, usize>,
    failure_by_symbol: std::collections::HashMap<String, usize>,
}

fn load_learning_bias_stats(workspace: &Path, crate_name: &str) -> LearningBiasStats {
    let raw = match fs::read_to_string(execution_learning_path(workspace)) {
        Ok(raw) => raw,
        Err(_) => return LearningBiasStats::default(),
    };
    let mut stats = LearningBiasStats::default();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("crate").and_then(|v| v.as_str()) != Some(crate_name) {
            continue;
        }
        let Some(symbol) = value
            .get("top_target")
            .and_then(|v| v.get("symbol"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        let verified = value
            .get("verification")
            .and_then(|v| v.get("verified"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let counter = if verified {
            &mut stats.success_by_symbol
        } else {
            &mut stats.failure_by_symbol
        };
        *counter.entry(symbol.to_string()).or_insert(0) += 1;
    }
    stats
}

fn apply_learning_bias_to_plan(
    plan: &mut crate::semantic::ExecutionPathPlan,
    stats: &LearningBiasStats,
) {
    for target in &mut plan.targets {
        let successes = *stats.success_by_symbol.get(&target.symbol).unwrap_or(&0) as i32;
        let failures = *stats.failure_by_symbol.get(&target.symbol).unwrap_or(&0) as i32;
        if successes > 0 {
            target.score -= successes * 5;
            target.reasons.push(format!("learned success x{successes}"));
        }
        if failures > 0 {
            target.score += failures * 8;
            target.reasons.push(format!("learned failure x{failures}"));
        }
    }
    plan.targets
        .sort_by(|a, b| a.score.cmp(&b.score).then(a.symbol.cmp(&b.symbol)));
    plan.top_target = plan.targets.first().cloned();
    plan.apply_patch_template = plan
        .top_target
        .as_ref()
        .and_then(crate::semantic::build_apply_patch_template_public);
}

fn append_execution_learning_record(workspace: &Path, record: &Value) -> Result<()> {
    let path = execution_learning_path(workspace);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create dir {}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    serde_json::to_writer(&mut file, record)
        .with_context(|| format!("write {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("newline {}", path.display()))?;
    Ok(())
}

fn parse_failure_location(out: &str) -> Option<(String, u32, u32)> {
    for line in out.lines() {
        let trimmed = line.trim();
        let candidate = trimmed.strip_prefix("--> ").unwrap_or(trimmed);
        let mut parts = candidate.rsplitn(3, ':');
        let col = parts.next()?.parse::<u32>().ok()?;
        let line_no = parts.next()?.parse::<u32>().ok()?;
        let file = parts.next()?.to_string();
        if file.ends_with(".rs") {
            return Some((file, line_no, col));
        }
    }
    None
}

fn verification_rebind(
    workspace: &Path,
    crate_name: &str,
    plan: Option<&crate::semantic::ExecutionPathPlan>,
    check_out: &str,
    test_out: &str,
) -> Option<Value> {
    let failure_output = if let Some(loc) = parse_failure_location(check_out) {
        Some((loc, "cargo_check"))
    } else {
        parse_failure_location(test_out).map(|loc| (loc, "cargo_test"))
    }?;
    let ((file, line, col), source) = failure_output;
    let idx = crate::semantic::SemanticIndex::load(workspace, crate_name).ok()?;
    let symbol = idx.symbol_at_file_line(&file, line);
    let rebound_plan = plan.and_then(|plan| {
        symbol
            .as_deref()
            .and_then(|sym| idx.execution_path_plan(&plan.from, sym).ok())
    });
    if let Some(rebound_plan) = &rebound_plan {
        let _ = persist_rebound_execution_plan(workspace, crate_name, rebound_plan);
    }
    Some(json!({
        "source": source,
        "file": file,
        "line": line,
        "col": col,
        "symbol": symbol,
        "rebound_path_fingerprint": rebound_plan.as_ref().map(|plan| plan.path_fingerprint.clone()),
        "rebound_from": rebound_plan.as_ref().map(|plan| plan.from.clone()),
        "rebound_to": rebound_plan.as_ref().map(|plan| plan.to.clone()),
    }))
}

// ---------------------------------------------------------------------------
// Semantic navigation handlers (backed by rustc graph.json)
// ---------------------------------------------------------------------------

fn load_semantic(
    workspace: &Path,
    action: &Value,
) -> anyhow::Result<crate::semantic::SemanticIndex> {
    let crate_name = semantic_crate_name(action);
    crate::semantic::SemanticIndex::load(workspace, &crate_name).map_err(|e| {
        anyhow!(
            "semantic index not available for crate '{crate_name}': {e}\n\
            Run `cargo build` (with canon-rustc-v2 wrapper) to generate the graph, or check \
            state/rustc/<crate>/graph.json exists."
        )
    })
}

fn semantic_crate_name(action: &Value) -> String {
    action
        .get("crate")
        .and_then(|v| v.as_str())
        .unwrap_or("canon_mini_agent")
        .replace('-', "_")
}

fn strip_semantic_crate_prefix<'a>(crate_name: &str, input: &'a str) -> &'a str {
    let mut s = input.trim();
    if let Some(rest) = s.strip_prefix("crate::") {
        s = rest;
    }
    if s == crate_name {
        return "";
    }
    if s.starts_with(crate_name) {
        let rest = &s[crate_name.len()..];
        if let Some(rest2) = rest.strip_prefix("::") {
            s = rest2;
        }
    }
    s
}

fn handle_semantic_map_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let idx = load_semantic(workspace, action)?;
    let crate_name = semantic_crate_name(action);
    let filter = action
        .get("filter")
        .and_then(|v| v.as_str())
        .map(|f| strip_semantic_crate_prefix(&crate_name, f))
        .filter(|f| !f.is_empty());
    let expand = action
        .get("expand_bodies")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let out = idx.semantic_map(filter, expand);
    Ok((false, out))
}

fn handle_symbol_window_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let idx = load_semantic(workspace, action)?;
    let crate_name = semantic_crate_name(action);
    let symbol = action
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("symbol_window requires a `symbol` field"))?;
    let symbol = strip_semantic_crate_prefix(&crate_name, symbol);
    if symbol.is_empty() {
        return Err(anyhow!("symbol_window requires a non-empty `symbol`"));
    }
    let out = idx.symbol_window(symbol)?;
    // Enrich with MIR data if available — eliminates the need for a follow-up rustc_mir call.
    let mir_suffix = idx
        .canonical_symbol_key(symbol)
        .ok()
        .and_then(|canonical| {
            idx.symbol_summaries()
                .into_iter()
                .find(|s| s.symbol == canonical)
                .filter(|s| s.mir_fingerprint.is_some())
                .map(|s| {
                    format!(
                        "\nmir: fingerprint={} blocks={} stmts={}",
                        s.mir_fingerprint.unwrap_or_default(),
                        s.mir_blocks.unwrap_or(0),
                        s.mir_stmts.unwrap_or(0),
                    )
                })
        })
        .unwrap_or_default();
    Ok((false, format!("{out}{mir_suffix}")))
}

fn handle_symbol_refs_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let idx = load_semantic(workspace, action)?;
    let crate_name = semantic_crate_name(action);
    let symbol = action
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("symbol_refs requires a `symbol` field"))?;
    let symbol = strip_semantic_crate_prefix(&crate_name, symbol);
    if symbol.is_empty() {
        return Err(anyhow!("symbol_refs requires a non-empty `symbol`"));
    }
    let expand = action
        .get("expand_bodies")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let out = if expand {
        idx.symbol_refs_expanded(symbol)?
    } else {
        idx.symbol_refs(symbol)?
    };
    Ok((false, out))
}

fn handle_symbol_path_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let idx = load_semantic(workspace, action)?;
    let crate_name = semantic_crate_name(action);
    let from = action
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("symbol_path requires a `from` field"))?;
    let to = action
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("symbol_path requires a `to` field"))?;
    let from = strip_semantic_crate_prefix(&crate_name, from);
    let to = strip_semantic_crate_prefix(&crate_name, to);
    if from.is_empty() || to.is_empty() {
        return Err(anyhow!("symbol_path requires non-empty `from` and `to`"));
    }
    let expand = action
        .get("expand_bodies")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let out = idx.symbol_path(from, to, expand)?;
    Ok((false, out))
}

fn handle_execution_path_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let idx = load_semantic(workspace, action)?;
    let crate_name = semantic_crate_name(action);
    let from = action
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("execution_path requires a `from` field"))?;
    let to = action
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("execution_path requires a `to` field"))?;
    let from = if from.starts_with("cfg::") {
        from
    } else {
        strip_semantic_crate_prefix(&crate_name, from)
    };
    let to = if to.starts_with("cfg::") {
        to
    } else {
        strip_semantic_crate_prefix(&crate_name, to)
    };
    if from.is_empty() || to.is_empty() {
        return Err(anyhow!("execution_path requires non-empty `from` and `to`"));
    }
    let expand = action
        .get("expand_bodies")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut plan = idx.execution_path_plan(from, to)?;
    let stats = load_learning_bias_stats(workspace, &crate_name);
    apply_learning_bias_to_plan(&mut plan, &stats);
    persist_execution_path_plan(workspace, &crate_name, &plan)?;
    let out = idx.render_execution_path_plan(&plan, expand);
    Ok((false, out))
}

fn handle_symbol_neighborhood_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let idx = load_semantic(workspace, action)?;
    let crate_name = semantic_crate_name(action);
    let symbol = action
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("symbol_neighborhood requires a `symbol` field"))?;
    let symbol = strip_semantic_crate_prefix(&crate_name, symbol);
    if symbol.is_empty() {
        return Err(anyhow!("symbol_neighborhood requires a non-empty `symbol`"));
    }
    let expand = action
        .get("expand_bodies")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let out = idx.symbol_neighborhood(symbol, expand)?;
    Ok((false, out))
}

#[cfg(test)]
mod semantic_input_normalization_tests {
    use super::strip_semantic_crate_prefix;

    #[test]
    fn strips_crate_prefixes() {
        assert_eq!(
            strip_semantic_crate_prefix("canon_mini_agent", "canon_mini_agent::constants"),
            "constants"
        );
        assert_eq!(
            strip_semantic_crate_prefix("canon_mini_agent", "crate::constants::EndpointSpec"),
            "constants::EndpointSpec"
        );
        assert_eq!(
            strip_semantic_crate_prefix("canon_mini_agent", "constants::EndpointSpec"),
            "constants::EndpointSpec"
        );
    }
}

/// Write the canonical stage graph artifact to `agent_state/orchestrator/stage_graph.json`.
/// Called automatically at agent-loop startup so the file is always present as a live artifact.
pub(crate) fn write_stage_graph(workspace: &Path) {
    if let Err(e) = write_stage_graph_inner(workspace, "agent_state/orchestrator/stage_graph.json")
    {
        eprintln!("[stage_graph] failed to write live artifact: {e}");
    }
}

fn write_stage_graph_inner(workspace: &Path, out_rel: &str) -> Result<()> {
    let out_path = {
        let p = std::path::Path::new(out_rel);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            workspace.join(p)
        }
    };
    if !out_path.starts_with(workspace) {
        bail!(
            "stage_graph output path must be under workspace: {}",
            out_path.display()
        );
    }
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create stage_graph parent dir {}", parent.display()))?;
    }
    let graph = build_stage_graph();
    let text = serde_json::to_string_pretty(&graph).unwrap_or_else(|_| graph.to_string());
    std::fs::write(&out_path, &text)
        .with_context(|| format!("write stage graph to {}", out_path.display()))?;
    Ok(())
}

fn stage_graph_node(
    id: &str,
    layer: u64,
    intent: &str,
    inputs: &[&str],
    outputs: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "layer": layer,
        "type": "stage",
        "intent": intent,
        "inputs": inputs,
        "outputs": outputs,
    })
}

fn stage_graph_edge(from: &str, to: &str, edge_type: &str) -> serde_json::Value {
    serde_json::json!({
        "from": from,
        "to": to,
        "type": edge_type,
    })
}

fn build_stage_graph_nodes() -> Vec<serde_json::Value> {
    vec![
        stage_graph_node("observe.input", 0, "collect state", &[], &["state"]),
        stage_graph_node(
            "orient.update",
            1,
            "update world model",
            &["state"],
            &["context"],
        ),
        stage_graph_node(
            "plan.generate",
            2,
            "generate actions",
            &["context"],
            &["actions"],
        ),
        stage_graph_node(
            "act.execute",
            3,
            "execute action",
            &["actions"],
            &["result"],
        ),
        stage_graph_node(
            "verify.check",
            4,
            "validate result",
            &["result"],
            &["verified"],
        ),
        stage_graph_node(
            "reward.score",
            5,
            "score outcome",
            &["verified"],
            &["feedback"],
        ),
    ]
}

fn build_stage_graph_edges() -> Vec<serde_json::Value> {
    vec![
        stage_graph_edge("observe.input", "orient.update", "call"),
        stage_graph_edge("orient.update", "plan.generate", "call"),
        stage_graph_edge("plan.generate", "act.execute", "call"),
        stage_graph_edge("act.execute", "verify.check", "call"),
        stage_graph_edge("verify.check", "reward.score", "call"),
        stage_graph_edge("verify.check", "plan.generate", "retry"),
        stage_graph_edge("orient.update", "plan.generate", "refine"),
    ]
}

fn build_stage_graph() -> serde_json::Value {
    serde_json::json!({
        "nodes": build_stage_graph_nodes(),
        "edges": build_stage_graph_edges(),
    })
}

fn handle_stage_graph_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let out_rel = action
        .get("out")
        .and_then(|v| v.as_str())
        .unwrap_or("agent_state/orchestrator/stage_graph.json");
    let out_path = {
        let p = std::path::Path::new(out_rel);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            workspace.join(p)
        }
    };
    if !out_path.starts_with(workspace) {
        bail!(
            "stage_graph output path must be under workspace: {}",
            out_path.display()
        );
    }
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create stage_graph parent dir {}", parent.display()))?;
    }

    let graph = build_stage_graph();
    let text = serde_json::to_string_pretty(&graph).unwrap_or_else(|_| graph.to_string());
    std::fs::write(&out_path, &text)
        .with_context(|| format!("write stage graph to {}", out_path.display()))?;
    Ok((false, text))
}

const BATCH_MUTATING: &[&str] = &[
    "message",
    "rename_symbol",
    "apply_patch",
    "run_command",
    "python",
    "cargo_test",
    "cargo_fmt",
    "cargo_clippy",
];

fn is_batch_item_mutating(kind: &str, item: &Value) -> bool {
    if BATCH_MUTATING.contains(&kind) {
        return true;
    }
    let op = item.get("op").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "plan" => op != "sorted_view",
        "objectives" => op != "read" && op != "sorted_view",
        "issue" => op != "read",
        "violation" => op != "read",
        _ => false,
    }
}

fn execute_batch_item(
    role: &str,
    step: usize,
    workspace: &Path,
    kind: &str,
    item: &Value,
) -> Result<(bool, String)> {
    match kind {
        "list_dir" => handle_list_dir_action(workspace, item),
        "read_file" => handle_read_file_action(role, step, workspace, item),
        "symbols_index" => handle_symbols_index_action(workspace, item),
        "symbols_rename_candidates" => handle_symbols_rename_candidates_action(workspace, item),
        "symbols_prepare_rename" => handle_symbols_prepare_rename_action(workspace, item),
        "objectives" => handle_objectives_action(workspace, item),
        "issue" => handle_issue_action(None, workspace, item),
        "violation" => handle_violation_action(None, workspace, item),
        "plan" => handle_plan_action(role, workspace, item),
        k @ ("rustc_hir" | "rustc_mir") => handle_rustc_action(role, step, k, workspace, item),
        k @ ("graph_call" | "graph_cfg") => {
            handle_graph_call_cfg_action(role, step, k, workspace, item)
        }
        k @ ("graph_dataflow" | "graph_reachability") => {
            handle_graph_reports_action(role, step, k, workspace, item)
        }
        "semantic_map" => handle_semantic_map_action(workspace, item),
        "stage_graph" => handle_stage_graph_action(workspace, item),
        "symbol_window" => handle_symbol_window_action(workspace, item),
        "symbol_refs" => handle_symbol_refs_action(workspace, item),
        "symbol_path" => handle_symbol_path_action(workspace, item),
        "execution_path" => handle_execution_path_action(workspace, item),
        "symbol_neighborhood" => handle_symbol_neighborhood_action(workspace, item),
        other => Ok((false, format!("unknown batchable action '{other}'"))),
    }
}

fn handle_batch_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    const MAX_BATCH: usize = 8;

    let items = match action.get("actions").and_then(|v| v.as_array()) {
        Some(arr) => arr.clone(),
        None => return Ok((false, "batch: `actions` array is required".to_string())),
    };

    if items.is_empty() {
        return Ok((
            false,
            "batch: `actions` array must not be empty".to_string(),
        ));
    }

    if items.len() > MAX_BATCH {
        return Ok((
            false,
            format!(
                "batch: too many items ({} > {MAX_BATCH}); split into smaller batches",
                items.len()
            ),
        ));
    }

    let total = items.len();
    let mut out = String::new();

    for (i, item) in items.iter().enumerate() {
        let n = i + 1;
        let kind = item
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if is_batch_item_mutating(kind, item) {
            append_rejected_batch_item(&mut out, n, total, kind, item);
            continue;
        }

        append_batch_item_result(&mut out, role, step, workspace, n, total, kind, item);
    }

    Ok((false, out))
}

fn batch_item_op_note(item: &Value) -> String {
    match item.get("op").and_then(|v| v.as_str()) {
        Some(op) => format!(" op={op}"),
        None => String::new(),
    }
}

fn append_rejected_batch_item(out: &mut String, n: usize, total: usize, kind: &str, item: &Value) {
    let op_note = batch_item_op_note(item);
    out.push_str(&format_rejected_batch_item(n, total, kind, &op_note));
}

fn format_rejected_batch_item(n: usize, total: usize, kind: &str, op_note: &str) -> String {
    format!(
        "[batch {n}/{total}: REJECTED {kind}{op_note}]\n\
         mutating action '{kind}{op_note}' is not allowed in batch\n\n"
    )
}

fn append_batch_item_result(
    out: &mut String,
    role: &str,
    step: usize,
    workspace: &Path,
    n: usize,
    total: usize,
    kind: &str,
    item: &Value,
) {
    out.push_str(&format!("[batch {n}/{total}: {kind}]\n"));
    match execute_batch_item(role, step, workspace, kind, item) {
        Ok((_done, result)) => append_batch_item_success(out, &result),
        Err(e) => out.push_str(&format!("ERROR: {e}\n")),
    }
    out.push('\n');
}

fn append_batch_item_success(out: &mut String, result: &str) {
    out.push_str(result);
    if !result.ends_with('\n') {
        out.push('\n');
    }
}

fn execute_action(
    role: &str,
    step: usize,
    action: &Value,
    workspace: &Path,
    _check_on_done: bool,
    mut writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let kind = action
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    tokio::task::block_in_place(|| match kind.as_str() {
        "message" => handle_message_action(role, step, action, writer.as_deref_mut()),
        "list_dir" => handle_list_dir_action(workspace, action),
        "read_file" => handle_read_file_action(role, step, workspace, action),
        "symbols_index" => handle_symbols_index_action(workspace, action),
        "symbols_rename_candidates" => handle_symbols_rename_candidates_action(workspace, action),
        "symbols_prepare_rename" => handle_symbols_prepare_rename_action(workspace, action),
        "rename_symbol" => handle_rename_symbol_action(role, step, workspace, action),
        "objectives" => {
            handle_objectives_action_with_writer(workspace, action, writer.as_deref_mut())
        }
        "issue" => handle_issue_action(writer, workspace, action),
        "violation" => handle_violation_action(writer, workspace, action),
        "apply_patch" => handle_apply_patch_action(role, step, writer, workspace, action),
        "run_command" => handle_run_command_action(role, step, workspace, action),
        "python" => handle_python_action(role, step, workspace, action),
        k @ ("rustc_hir" | "rustc_mir") => handle_rustc_action(role, step, k, workspace, action),
        k @ ("graph_call" | "graph_cfg") => {
            handle_graph_call_cfg_action(role, step, k, workspace, action)
        }
        k @ ("graph_dataflow" | "graph_reachability") => {
            handle_graph_reports_action(role, step, k, workspace, action)
        }
        "cargo_test" => handle_cargo_test_action(role, step, workspace, action),
        "cargo_fmt" => handle_cargo_fmt_action(role, step, workspace, action),
        "cargo_clippy" => handle_cargo_clippy_action(role, step, workspace, action),
        "plan" => handle_plan_action(role, workspace, action),
        "semantic_map" => handle_semantic_map_action(workspace, action),
        "stage_graph" => handle_stage_graph_action(workspace, action),
        "symbol_window" => handle_symbol_window_action(workspace, action),
        "symbol_refs" => handle_symbol_refs_action(workspace, action),
        "symbol_path" => handle_symbol_path_action(workspace, action),
        "execution_path" => handle_execution_path_action(workspace, action),
        "symbol_neighborhood" => handle_symbol_neighborhood_action(workspace, action),
        "lessons" => {
            crate::lessons::handle_lessons_action_with_writer(workspace, action, writer.as_deref_mut())
        }
        "invariants" => {
            crate::invariants::handle_invariants_action_with_writer(
                workspace,
                action,
                writer.as_deref_mut(),
                role,
            )
        }
        "batch" => handle_batch_action(role, step, workspace, action),
        other => {
            crate::blockers::record_action_failure_with_writer(
                workspace,
                writer.as_deref_mut(),
                role,
                other,
                &format!("unsupported action '{other}'"),
                None,
            );
            Ok((
                false,
                format!(
                    "unsupported action '{other}' — use one of: {}",
                    crate::tool_schema::predicted_action_name_list().join(", ")
                ),
            ))
        }
    })
}

/// Execute a single tool action with the same semantics as the main agent loop.
///
/// This is exported for small "capability binaries" that compose tool actions via stdin/stdout.
pub fn execute_action_capability(
    role: &str,
    step: usize,
    action: &Value,
    workspace: &Path,
    check_on_done: bool,
) -> Result<(bool, String)> {
    execute_action(role, step, action, workspace, check_on_done, None)
}

fn sanitize_inbound_target(role: &str, to: &str) -> String {
    if to == "planner" || to == "executor" {
        return to.to_string();
    }
    if role == "planner" || role == "mini_planner" {
        "executor".to_string()
    } else {
        "planner".to_string()
    }
}

fn resolve_inbound_message_target(role: &str, step: usize, action: &Value) -> Option<String> {
    let to_raw = action.get("to").and_then(|v| v.as_str())?;
    if to_raw.trim().is_empty() {
        return None;
    }
    let normalized_to_raw = to_raw
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let to = sanitize_inbound_target(role, &normalized_to_raw);
    let normalized_role = role
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    if normalized_role == to {
        if is_allowed_self_addressed_message(action, &normalized_role, &to) {
            return None;
        }
        eprintln!(
            "[{role}] step={step} invalid self-addressed message to `{to}` — canonical ingress suppressed"
        );
        return None;
    }
    Some(to)
}

fn persist_inbound_message(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
    full_message: &str,
    mut writer: Option<&mut CanonicalWriter>,
) {
    let Some(to) = resolve_inbound_message_target(role, step, action) else {
        return;
    };
    let message_signature = artifact_write_signature(&[
        "inbound_message",
        role,
        &to,
        &full_message.len().to_string(),
        full_message,
    ]);
    if let Err(err) = record_effect_for_workspace(
        workspace,
        crate::events::EffectEvent::InboundMessageRecorded {
            from_role: role.to_string(),
            to_role: to.clone(),
            message: full_message.to_string(),
            signature: message_signature.clone(),
        },
    ) {
        eprintln!(
            "[{role}] step={} failed to record canonical inbound message for {}: {}",
            step, to, err
        );
    }
    if let Some(w) = writer.as_deref_mut() {
        w.apply(ControlEvent::InboundMessageQueued {
            role: to.clone(),
            content: full_message.to_string(),
            signature: message_signature.clone(),
        });
        let wake_signature = artifact_write_signature(&["wake", &to, &now_ms().to_string()]);
        w.apply(ControlEvent::WakeSignalQueued {
            role: to.clone(),
            signature: wake_signature,
            ts_ms: now_ms(),
        });
    }
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let path = agent_state_dir.join(format!("last_message_to_{to}.json"));
    if let Err(err) = write_projection_with_workspace_effects(
        workspace,
        &path,
        &format!("agent_state/last_message_to_{to}.json"),
        &format!("handoff_message:{role}:{to}"),
        full_message,
    ) {
        eprintln!(
            "[{role}] step={} failed to persist inbound message for {}: {}",
            step, to, err
        );
        log_error_event(
            role,
            "persist_inbound_message",
            Some(step),
            &format!("failed to persist inbound message for {}: {}", to, err),
            Some(json!({ "path": path.to_string_lossy(), "to": to })),
        );
        if let Some(workspace) = agent_state_dir.parent() {
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                role,
                "handoff_delivery",
                &format!("failed to write message file for {to}: {err}"),
                None,
            );
        }
    }
    // Wakeup flag projection retired; wake routing is canonicalized via
    // ControlEvent::WakeSignalQueued.
}

fn build_execute_logged_action_error_text(action_kind: &str, error: &anyhow::Error) -> String {
    if action_kind == "plan" {
        format!(
            "Error executing action: {error}\n\nPlan tool examples:\n{}\n{}\n{{\"action\":\"plan\",\"op\":\"update_task\",\"task\":{{\"id\":\"T4\",\"status\":\"done\"}},\"rationale\":\"Update a task by id using task payload.\"}}\n\nTo mark a task done, use update_task or set_task_status. set_plan_status changes only PLAN.status.",
            plan_set_task_status_action_example(
                "T1",
                "in_progress",
                "Update one task status in PLAN.json."
            ),
            plan_set_plan_status_action_example(
                "in_progress",
                "Update top-level PLAN.json status."
            ),
        )
    } else {
        format!("Error executing action: {error}")
    }
}

fn record_execute_logged_action_failure(
    workspace: &Path,
    writer: Option<&mut CanonicalWriter>,
    role: &str,
    endpoint: &LlmEndpoint,
    prompt_kind: &str,
    step: usize,
    command_id: &str,
    action: &Value,
    error: &anyhow::Error,
) -> String {
    let action_kind = action
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let task_id = action.get("task_id").and_then(|v| v.as_str());
    let mut writer = writer;
    crate::blockers::record_action_failure_with_writer(
        workspace,
        writer.as_deref_mut(),
        role,
        action_kind,
        &error.to_string(),
        task_id,
    );
    let err_text = build_execute_logged_action_error_text(action_kind, error);
    log_action_result(
        writer.as_deref_mut(),
        role,
        endpoint,
        prompt_kind,
        step,
        command_id,
        action,
        false,
        &err_text,
    );
    log_error_event(
        role,
        "execute_logged_action",
        Some(step),
        &format!("execute_logged_action error: {error}"),
        Some(json!({
            "prompt_kind": prompt_kind,
            "command_id": command_id,
            "action": action.get("action").and_then(|v| v.as_str()),
        })),
    );
    err_text
}

pub(crate) fn execute_logged_action(
    role: &str,
    prompt_kind: &str,
    endpoint: &LlmEndpoint,
    workspace: &Path,
    step: usize,
    command_id: &str,
    action: &Value,
    check_on_done: bool,
    mut writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    log_action_event(role, endpoint, prompt_kind, step, command_id, action);
    match execute_action(
        role,
        step,
        action,
        workspace,
        check_on_done,
        writer.as_deref_mut(),
    ) {
        Ok((done, out)) => {
            log_action_result(
                writer.as_deref_mut(),
                role,
                endpoint,
                prompt_kind,
                step,
                command_id,
                action,
                true,
                &out,
            );
            Ok((done, out))
        }
        Err(e) => {
            let err_text = record_execute_logged_action_failure(
                workspace,
                writer.as_deref_mut(),
                role,
                endpoint,
                prompt_kind,
                step,
                command_id,
                action,
                &e,
            );
            eprintln!("[{role}] step={} error: {e}", step);
            Ok((false, err_text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::handle_apply_patch_action;
    use super::handle_execution_path_action;
    use super::handle_issue_action;
    use super::handle_objectives_action;
    use super::handle_plan_action;
    use super::handle_read_file_action;
    use super::handle_rename_symbol_action;
    use super::handle_stage_graph_action;
    use super::handle_symbols_index_action;
    use super::handle_symbols_prepare_rename_action;
    use super::handle_symbols_rename_candidates_action;
    use super::is_allowed_self_addressed_message;
    use super::stable_hash_hex;
    use super::EvidenceReceipt;
    use crate::constants::set_agent_state_dir;
    use crate::constants::set_workspace;
    use crate::constants::{ISSUES_FILE, MASTER_PLAN_FILE};
    use crate::issues::IssuesFile;
    use crate::logging::init_log_paths;
    use crate::logging::now_ms;
    use serde_json::json;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::OnceLock;

    fn test_state_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn write_minimal_graph_for_ident(
        workspace: &std::path::Path,
        crate_name: &str,
        symbol_key: &str,
        file: &std::path::Path,
        source: &str,
        ident: &str,
    ) {
        let mut refs = Vec::new();
        for (lo, _) in source.match_indices(ident) {
            let hi = lo + ident.len();
            let prefix = &source[..lo];
            let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
            let col = prefix.bytes().rev().take_while(|b| *b != b'\n').count();
            refs.push(serde_json::json!({
                "file": file.display().to_string(),
                "line": line as u32,
                "col": col as u32,
                "lo": lo as u32,
                "hi": hi as u32,
            }));
        }
        let graph = serde_json::json!({
            "nodes": {
                symbol_key: {
                    "kind": "fn",
                    "refs": refs,
                    "fields": [],
                }
            },
            "edges": []
        });
        let path = workspace
            .join("state/rustc")
            .join(crate_name)
            .join("graph.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&graph).unwrap()).unwrap();
    }

    fn fresh_test_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("canon-mini-agent-{name}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("agent_state")).unwrap();
        dir
    }

    fn write_test_evidence_receipt(
        workspace: &std::path::Path,
        state_dir: &std::path::Path,
        id: &str,
    ) {
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());
        std::fs::create_dir_all(state_dir).unwrap();
        let receipt = EvidenceReceipt {
            id: id.to_string(),
            ts_ms: now_ms(),
            actor: "planner".to_string(),
            step: 1,
            action: "python".to_string(),
            path: Some(ISSUES_FILE.to_string()),
            abs_path: Some(workspace.join(ISSUES_FILE).display().to_string()),
            meta: json!({"test": true}),
            output_hash: stable_hash_hex("test-output"),
        };
        std::fs::write(
            state_dir.join("evidence_receipts.jsonl"),
            format!("{}\n", serde_json::to_string(&receipt).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn issue_upsert_alias_creates_and_updates_issue() {
        let _guard = test_state_lock().lock().unwrap();
        let workspace = fresh_test_dir("issue-upsert");
        let state_dir = workspace.join("agent_state");
        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-upsert-1");

        let create = json!({
            "action": "issue",
            "op": "upsert",
            "evidence_receipts": ["rcpt-issue-upsert-1"],
            "issue": {
                "id": "ISS-transport",
                "title": "Transport failure",
                "status": "open",
                "priority": "high",
                "kind": "bug",
                "description": "first version",
                "evidence": ["tlog blocker"],
                "discovered_by": "planner"
            },
            "rationale": "record runtime blocker",
            "predicted_next_actions": []
        });
        let (_, create_out) = handle_issue_action(None, &workspace, &create).unwrap();
        assert!(create_out.contains("added `ISS-transport`"));

        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-upsert-2");
        let update = json!({
            "action": "issue",
            "op": "upsert",
            "issue_id": "ISS-transport",
            "evidence_receipts": ["rcpt-issue-upsert-2"],
            "issue": {
                "id": "ISS-transport",
                "title": "Transport failure",
                "status": "in_progress",
                "priority": "medium",
                "kind": "bug",
                "description": "updated version",
                "evidence": ["tlog blocker", "fresh receipt"],
                "discovered_by": "planner"
            },
            "rationale": "refresh runtime blocker",
            "predicted_next_actions": []
        });
        let (_, update_out) = handle_issue_action(None, &workspace, &update).unwrap();
        assert!(update_out.contains("updated `ISS-transport`"));

        let issues: IssuesFile =
            serde_json::from_str(&std::fs::read_to_string(workspace.join(ISSUES_FILE)).unwrap())
                .unwrap();
        let issue = issues
            .issues
            .iter()
            .find(|issue| issue.id == "ISS-transport")
            .unwrap();
        assert_eq!(issue.status, "in_progress");
        assert_eq!(issue.priority, "medium");
        assert_eq!(issue.description, "updated version");
    }

    #[test]
    fn issue_resolve_alias_marks_issue_resolved() {
        let _guard = test_state_lock().lock().unwrap();
        let workspace = fresh_test_dir("issue-resolve");
        let state_dir = workspace.join("agent_state");
        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-resolve-1");

        let create = json!({
            "action": "issue",
            "op": "create",
            "evidence_receipts": ["rcpt-issue-resolve-1"],
            "issue": {
                "id": "ISS-recover",
                "title": "Recovered completion",
                "status": "open",
                "priority": "high",
                "kind": "bug",
                "description": "needs closure",
                "evidence": ["tlog blocker"],
                "discovered_by": "planner"
            },
            "rationale": "seed issue",
            "predicted_next_actions": []
        });
        handle_issue_action(None, &workspace, &create).unwrap();

        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-resolve-2");
        let resolve = json!({
            "action": "issue",
            "op": "resolve",
            "issue_id": "ISS-recover",
            "evidence_receipts": ["rcpt-issue-resolve-2"],
            "rationale": "close resolved issue",
            "predicted_next_actions": []
        });
        let (_, out) = handle_issue_action(None, &workspace, &resolve).unwrap();
        assert!(out.contains("issue resolve ok"));

        let issues: IssuesFile =
            serde_json::from_str(&std::fs::read_to_string(workspace.join(ISSUES_FILE)).unwrap())
                .unwrap();
        let issue = issues
            .issues
            .iter()
            .find(|issue| issue.id == "ISS-recover")
            .unwrap();
        assert_eq!(issue.status, "resolved");
    }

    #[test]
    fn only_solo_result_complete_may_self_route() {
        let solo = json!({
            "action": "message",
            "from": "solo",
            "to": "solo",
            "type": "result",
            "status": "complete",
            "payload": {"summary": "done"}
        });
        assert!(is_allowed_self_addressed_message(&solo, "solo", "solo"));

        let planner = json!({
            "action": "message",
            "from": "planner",
            "to": "planner",
            "type": "blocker",
            "status": "blocked",
            "payload": {"summary": "blocked"}
        });
        assert!(!is_allowed_self_addressed_message(
            &planner, "planner", "planner"
        ));
    }

    fn write_minimal_graph_with_def_and_mir(
        workspace: &std::path::Path,
        crate_name: &str,
        symbol_key: &str,
        file: &std::path::Path,
        source: &str,
        ident: &str,
    ) {
        let lo = source.find(ident).expect("ident present");
        let hi = lo + ident.len();
        let prefix = &source[..lo];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
        let col = prefix.bytes().rev().take_while(|b| *b != b'\n').count();
        let def = serde_json::json!({
            "file": file.display().to_string(),
            "line": line as u32,
            "col": col as u32,
            "lo": lo as u32,
            "hi": hi as u32,
        });
        let graph = serde_json::json!({
            "nodes": {
                symbol_key: {
                    "kind": "fn",
                    "def": def,
                    "refs": [],
                    "signature": "fn test()",
                    "mir": { "fingerprint": "fp1", "blocks": 2, "stmts": 3 },
                    "fields": [],
                }
            },
            "edges": []
        });
        let path = workspace
            .join("state/rustc")
            .join(crate_name)
            .join("graph.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&graph).unwrap()).unwrap();
    }

    #[test]
    fn diagnostics_apply_patch_rejects_unvalidated_ranked_failures() {
        let tmp = fresh_test_dir("rejects-unvalidated-ranked-failures");
        std::fs::write(
            tmp.join("DIAGNOSTICS.json"),
            "{\"status\":\"healthy\",\"ranked_failures\":[]}",
        )
        .unwrap();
        let action = json!({
            "patch": "*** Begin Patch\n*** Delete File: DIAGNOSTICS.json\n*** Add File: DIAGNOSTICS.json\n+{\n+  \"status\": \"critical_failure\",\n+  \"summary\": \"stale issue\",\n+  \"ranked_failures\": [\n+    {\n+      \"id\": \"D1\",\n+      \"evidence\": [\"old report without source validation\"]\n+    }\n+  ]\n+}\n*** End Patch"
        });

        let (_done, out) =
            handle_apply_patch_action("diagnostics", 1, None, &tmp, &action).unwrap();

        assert!(out.contains("derived cache view"));
        let persisted: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.join("DIAGNOSTICS.json")).unwrap())
                .unwrap();
        assert_eq!(
            persisted.get("status").and_then(|v| v.as_str()),
            Some("healthy")
        );
        assert_eq!(
            persisted
                .get("ranked_failures")
                .and_then(|v| v.as_array())
                .map(|entries| entries.len()),
            Some(0)
        );
    }

    #[test]
    fn read_file_result_surfaces_evidence_receipt_id() {
        let tmp = fresh_test_dir("read-file-receipt");
        let target = tmp.join("sample.txt");
        std::fs::write(&target, "alpha\nbeta\n").unwrap();

        let action = json!({"path": "sample.txt"});
        let (_done, out) = handle_read_file_action("diagnostics", 1, &tmp, &action).unwrap();

        assert!(out.contains("Evidence receipt: rcpt-"), "unexpected: {out}");
        assert!(out.contains("alpha"), "unexpected: {out}");
    }

    #[test]
    fn stage_graph_writes_default_artifact() {
        let tmp = fresh_test_dir("stage-graph");
        init_log_paths("stage-graph-test");
        let action = json!({});
        let (_done, out) = handle_stage_graph_action(&tmp, &action).unwrap();
        assert!(out.contains("\"nodes\""));
        assert!(out.contains("observe.input"));
        let path = tmp.join("agent_state/orchestrator/stage_graph.json");
        assert!(path.exists(), "expected stage graph at {}", path.display());
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            parsed
                .get("nodes")
                .and_then(|v| v.as_array())
                .unwrap()
                .len()
                >= 6
        );
        assert!(
            parsed
                .get("edges")
                .and_then(|v| v.as_array())
                .unwrap()
                .len()
                >= 7
        );
    }

    #[test]
    fn diagnostics_apply_patch_allows_source_validated_ranked_failures() {
        let tmp = fresh_test_dir("allows-source-validated-ranked-failures");
        std::fs::write(
            tmp.join("DIAGNOSTICS.json"),
            r#"{"status":"healthy","inputs_scanned":[],"ranked_failures":[],"planner_handoff":[]}"#,
        )
        .unwrap();
        let action = json!({
            "patch": "*** Begin Patch\n*** Delete File: DIAGNOSTICS.json\n*** Add File: DIAGNOSTICS.json\n+{\n+  \"status\": \"critical_failure\",\n+  \"inputs_scanned\": [\"agent_state/default/log.jsonl\"],\n+  \"ranked_failures\": [\n+    {\n+      \"id\": \"D1\",\n+      \"impact\": \"high\",\n+      \"signal\": \"read_file src/app.rs verified against current source\",\n+      \"evidence\": [\"read_file src/app.rs:1-50 — confirmed missing check\"],\n+      \"root_cause\": \"missing validation\",\n+      \"repair_targets\": [\"src/app.rs\"]\n+    }\n+  ],\n+  \"planner_handoff\": [\"Fix missing validation in src/app.rs\"]\n+}\n*** End Patch"
        });

        let (_done, out) =
            handle_apply_patch_action("diagnostics", 1, None, &tmp, &action).unwrap();

        assert!(out.contains("derived cache view"), "unexpected: {out}");
        let persisted: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.join("DIAGNOSTICS.json")).unwrap())
                .unwrap();
        assert_eq!(
            persisted.get("status").and_then(|v| v.as_str()),
            Some("healthy")
        );
        assert_eq!(
            persisted
                .get("ranked_failures")
                .and_then(|v| v.as_array())
                .map(|entries| entries.len()),
            Some(0)
        );
    }

    #[test]
    fn execution_path_persists_latest_plan_artifact() {
        let tmp = fresh_test_dir("execution-path-artifact");
        let file = tmp.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let src = "fn validate() {}\n";
        std::fs::write(&file, src).unwrap();
        write_minimal_graph_with_def_and_mir(
            &tmp,
            "canon_mini_agent",
            "app::validate",
            &file,
            src,
            "validate",
        );
        let action = json!({
            "action": "execution_path",
            "crate": "canon_mini_agent",
            "from": "app::validate",
            "to": "app::validate",
            "rationale": "Persist a repair plan for the selected symbol."
        });

        let (_done, out) = handle_execution_path_action(&tmp, &action).unwrap();

        assert!(out.contains("Repair plan:"), "unexpected: {out}");
        let latest = tmp
            .join("state")
            .join("reports")
            .join("execution_path")
            .join("canon_mini_agent.latest.json");
        assert!(latest.exists(), "missing {}", latest.display());
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&latest).unwrap()).unwrap();
        assert_eq!(
            parsed.get("from").and_then(|v| v.as_str()),
            Some("app::validate")
        );
        assert_eq!(
            parsed
                .get("top_target")
                .and_then(|v| v.get("symbol"))
                .and_then(|v| v.as_str()),
            Some("app::validate")
        );
        assert!(parsed.get("apply_patch_template").is_some());
    }

    #[test]
    fn execution_path_applies_learning_bias_from_prior_success() {
        let tmp = fresh_test_dir("execution-path-learning-bias");
        let file = tmp.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let src = "fn validate() {}\n";
        std::fs::write(&file, src).unwrap();
        write_minimal_graph_with_def_and_mir(
            &tmp,
            "canon_mini_agent",
            "app::validate",
            &file,
            src,
            "validate",
        );
        let reports_dir = tmp.join("state").join("reports");
        std::fs::create_dir_all(&reports_dir).unwrap();
        std::fs::write(
            reports_dir.join("execution_learning.jsonl"),
            concat!(
                "{\"crate\":\"canon_mini_agent\",\"top_target\":{\"symbol\":\"app::validate\"},",
                "\"verification\":{\"verified\":true}}\n"
            ),
        )
        .unwrap();
        let action = json!({
            "action": "execution_path",
            "crate": "canon_mini_agent",
            "from": "app::validate",
            "to": "app::validate",
            "rationale": "Prefer symbols that succeeded on similar prior patches."
        });

        let (_done, out) = handle_execution_path_action(&tmp, &action).unwrap();

        assert!(out.contains("learned success x1"), "unexpected: {out}");
    }

    #[test]
    fn rename_symbol_renames_via_semantic_spans() {
        let tmp = fresh_test_dir("rename-symbol-success");
        let file = tmp.join("lib.rs");
        let src = "fn foo() {\n    foo();\n}\n";
        std::fs::write(&file, src).unwrap();
        write_minimal_graph_for_ident(&tmp, "canon_mini_agent", "foo", &file, src, "foo");
        let action = json!({
            "crate": "canon_mini_agent",
            "old_symbol": "foo",
            "new_symbol": "bar",
            "question": "rename foo to bar",
            "rationale": "Rename symbol",
            "predicted_next_actions": [
                {"action": "cargo_test", "intent": "verify"},
                {"action": "run_command", "intent": "check"}
            ]
        });

        let (_done, out) = handle_rename_symbol_action("solo", 1, &tmp, &action).unwrap();
        assert!(out.contains("rename_symbol ok"));
        let persisted = std::fs::read_to_string(&file).unwrap();
        assert!(persisted.contains("fn bar()"));
        assert!(persisted.contains("bar();"));
        assert!(!persisted.contains("foo"));
    }

    #[test]
    fn rename_symbol_rejects_span_mismatch_when_graph_is_stale() {
        let tmp = fresh_test_dir("rename-symbol-old-name-mismatch");
        let file = tmp.join("lib.rs");
        let src = "fn baz() {}\n";
        std::fs::write(&file, src).unwrap();
        // Graph claims `foo` spans exist at offsets where the file contains `baz`.
        write_minimal_graph_for_ident(&tmp, "canon_mini_agent", "foo", &file, src, "baz");
        let action = json!({
            "crate": "canon_mini_agent",
            "old_symbol": "foo",
            "new_symbol": "bar",
            "question": "rename foo to bar",
            "rationale": "Rename symbol",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "re-check position"},
                {"action": "message", "intent": "report blocker"}
            ]
        });

        let err = handle_rename_symbol_action("solo", 1, &tmp, &action)
            .unwrap_err()
            .to_string();
        assert!(err.contains("span mismatch"), "unexpected: {err}");
    }

    #[test]
    fn symbols_index_writes_deterministic_sorted_unique_output() {
        let tmp = fresh_test_dir("symbols-index-deterministic");
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(
            tmp.join("src/a.rs"),
            "pub struct Alpha {}\nimpl Alpha { pub fn new() -> Self { Self {} } }\n",
        )
        .unwrap();
        std::fs::write(tmp.join("src/b.rs"), "pub enum Beta { One }\n").unwrap();

        let action = json!({
            "path": "src",
            "out": "state/symbols.json",
            "rationale": "Index symbols",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "inspect symbols"},
                {"action": "rename_symbol", "intent": "rename a selected symbol"}
            ]
        });
        let (_done, out) = handle_symbols_index_action(&tmp, &action).unwrap();
        assert!(out.contains("symbols_index ok"));

        let symbols_path = tmp.join("state/symbols.json");
        assert!(symbols_path.exists());
        let raw = std::fs::read_to_string(&symbols_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.get("version").and_then(|v| v.as_u64()), Some(1));
        let symbols = parsed
            .get("symbols")
            .and_then(|v| v.as_array())
            .expect("symbols array");
        assert!(!symbols.is_empty());
        let mut prev: Option<(String, u64, u64, String, String)> = None;
        for sym in symbols {
            let file = sym.get("file").and_then(|v| v.as_str()).unwrap_or("");
            let start = sym
                .get("span")
                .and_then(|s| s.get("start"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let end = sym
                .get("span")
                .and_then(|s| s.get("end"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let kind = sym.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let name = sym.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let key = (
                file.to_string(),
                start,
                end,
                kind.to_string(),
                name.to_string(),
            );
            if let Some(prev_key) = prev.take() {
                assert!(
                    prev_key < key,
                    "symbols output should be strictly sorted and unique"
                );
            }
            prev = Some(key);
        }
    }

    #[test]
    fn rustc_actions_read_graph_json_when_present() {
        let tmp = fresh_test_dir("rustc-graph-actions");
        let file = tmp.join("lib.rs");
        let src = "fn foo() { foo(); }\n";
        std::fs::write(&file, src).unwrap();
        write_minimal_graph_with_def_and_mir(
            &tmp,
            "canon_mini_agent",
            "app::foo",
            &file,
            src,
            "foo",
        );

        let action = json!({
            "crate": "canon_mini_agent",
            "mode": "hir-tree",
            "symbol": "app::foo",
            "extra": ""
        });
        let (_done, out_hir) =
            super::handle_rustc_action("solo", 1, "rustc_hir", &tmp, &action).unwrap();
        assert!(
            out_hir.contains("rustc_hir ok (graph)"),
            "unexpected: {out_hir}"
        );
        assert!(out_hir.contains("fn foo"), "unexpected: {out_hir}");

        let action = json!({
            "crate": "canon_mini_agent",
            "mode": "mir",
            "extra": ""
        });
        let (_done, out_mir) =
            super::handle_rustc_action("solo", 1, "rustc_mir", &tmp, &action).unwrap();
        assert!(
            out_mir.contains("rustc_mir ok (graph)"),
            "unexpected: {out_mir}"
        );
        assert!(out_mir.contains("app::foo"), "unexpected: {out_mir}");
        assert!(out_mir.contains("fp1"), "unexpected: {out_mir}");
    }

    #[test]
    fn rustc_mir_supports_symbol_field_for_focused_summary() {
        let tmp = fresh_test_dir("rustc-mir-symbol");
        let file = tmp.join("lib.rs");
        let src = "fn foo() { foo(); }\n";
        std::fs::write(&file, src).unwrap();
        write_minimal_graph_with_def_and_mir(
            &tmp,
            "canon_mini_agent",
            "tools::handle_objectives_action",
            &file,
            src,
            "foo",
        );

        let action = json!({
            "crate": "canon_mini_agent",
            "mode": "mir",
            "symbol": "handle_objectives_action",
            "extra": ""
        });
        let (_done, out) =
            super::handle_rustc_action("solo", 1, "rustc_mir", &tmp, &action).unwrap();
        assert!(
            out.contains("symbol: handle_objectives_action"),
            "unexpected: {out}"
        );
        assert!(out.contains("rank_by_blocks:"), "unexpected: {out}");
        assert!(out.contains("fingerprint=fp1"), "unexpected: {out}");
    }

    #[test]
    fn symbols_rename_candidates_derives_heuristic_candidates() {
        let tmp = fresh_test_dir("symbols-rename-candidates");
        std::fs::create_dir_all(tmp.join("state")).unwrap();
        let symbols_json = serde_json::json!({
            "version": 1,
            "symbols": [
                {"name":"tmp","kind":"function","file":"src/a.rs","span":{"start":1,"end":4,"line":1,"column":1,"end_line":1,"end_column":4}},
                {"name":"get_data","kind":"function","file":"src/a.rs","span":{"start":10,"end":18,"line":2,"column":1,"end_line":2,"end_column":9}},
                {"name":"fetch_data","kind":"function","file":"src/b.rs","span":{"start":20,"end":30,"line":3,"column":1,"end_line":3,"end_column":11}},
                {"name":"clear_name","kind":"function","file":"src/c.rs","span":{"start":40,"end":50,"line":4,"column":1,"end_line":4,"end_column":11}}
            ]
        });
        std::fs::write(
            tmp.join("state/symbols.json"),
            serde_json::to_string_pretty(&symbols_json).unwrap(),
        )
        .unwrap();
        let action = json!({
            "symbols_path": "state/symbols.json",
            "out": "state/rename_candidates.json",
            "rationale": "derive candidates",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "inspect"},
                {"action": "rename_symbol", "intent": "apply"}
            ]
        });

        let (_done, out) = handle_symbols_rename_candidates_action(&tmp, &action).unwrap();
        assert!(out.contains("symbols_rename_candidates ok"));
        let raw = std::fs::read_to_string(tmp.join("state/rename_candidates.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let candidates = parsed
            .get("candidates")
            .and_then(|v| v.as_array())
            .expect("candidates array");
        assert!(!candidates.is_empty());
        assert!(candidates
            .iter()
            .any(|c| c.get("name").and_then(|v| v.as_str()) == Some("tmp")));
        assert!(candidates.iter().any(|c| {
            c.get("name").and_then(|v| v.as_str()) == Some("get_data")
                && c.get("reasons")
                    .and_then(|v| v.as_array())
                    .is_some_and(|arr| {
                        arr.iter().any(|r| {
                            r.as_str()
                                .unwrap_or("")
                                .contains("inconsistent verb prefix")
                        })
                    })
        }));
    }

    #[test]
    fn symbols_prepare_rename_writes_ready_action_payload() {
        let tmp = fresh_test_dir("symbols-prepare-rename");
        std::fs::create_dir_all(tmp.join("state")).unwrap();
        let candidates_json = serde_json::json!({
            "version": 1,
            "source_symbols_path": "state/symbols.json",
            "candidates": [
                {"name":"tmp","kind":"function","file":"src/a.rs","span":{"start":1,"end":4,"line":10,"column":5,"end_line":10,"end_column":8},"score":55,"reasons":["name is ambiguous/generic"]}
            ]
        });
        std::fs::write(
            tmp.join("state/rename_candidates.json"),
            serde_json::to_string_pretty(&candidates_json).unwrap(),
        )
        .unwrap();
        let action = json!({
            "candidates_path": "state/rename_candidates.json",
            "index": 0,
            "out": "state/next_rename_action.json",
            "rationale": "prepare rename action",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "inspect payload"},
                {"action": "rename_symbol", "intent": "execute"}
            ]
        });

        let (_done, out) = handle_symbols_prepare_rename_action(&tmp, &action).unwrap();
        assert!(out.contains("symbols_prepare_rename ok"));
        let raw = std::fs::read_to_string(tmp.join("state/next_rename_action.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.get("version").and_then(|v| v.as_u64()), Some(1));
        let rename_action = parsed.get("rename_action").expect("rename_action");
        assert_eq!(
            rename_action
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "rename_symbol"
        );
        // v2 payload shape: ensure symbol-based fields exist instead of span-based fields
        assert!(rename_action.get("old_symbol").is_some());
        assert!(rename_action.get("new_symbol").is_some());
        // ensure deprecated fields are not present
        assert!(rename_action.get("path").is_none());
        assert!(rename_action.get("line").is_none());
        assert!(rename_action.get("column").is_none());
    }

    #[test]
    fn plan_update_task_rejects_reopened_task_without_regression_linkage() {
        let tmp = fresh_test_dir("rejects-reopened-task-without-regression-linkage");
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Regression-linked task",
      "status": "done",
      "priority": 1,
      "steps": ["existing regression coverage"]
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "update_task",
            "task": {
                "id": "T1",
                "status": "in_progress",
                "steps": ["resume implementation without linked test"]
            },
            "rationale": "Exercise reopened-task enforcement"
        });

        let err = handle_plan_action("solo", &tmp, &action)
            .unwrap_err()
            .to_string();

        assert!(err.contains("reopened task T1 must include regression-test linkage"));
    }

    #[test]
    fn plan_update_task_allows_reopened_task_with_regression_linkage() {
        let tmp = fresh_test_dir("allows-reopened-task-with-regression-linkage");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Regression-linked task",
      "status": "done",
      "priority": 1,
      "steps": ["existing regression coverage"]
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "update_task",
            "task": {
                "id": "T1",
                "status": "in_progress",
                "steps": ["add regression test linkage before reopening"]
            },
            "rationale": "Exercise reopened-task allowance"
        });

        let (_done, out) = handle_plan_action("solo", &tmp, &action).unwrap();

        assert!(out.contains("plan ok"));
        let persisted = std::fs::read_to_string(tmp.join(MASTER_PLAN_FILE)).unwrap();
        assert!(persisted.contains("\"status\": \"in_progress\""));
        assert!(persisted.contains("add regression test linkage before reopening"));
    }

    #[test]
    fn plan_set_plan_status_rejects_done_when_any_task_is_incomplete() {
        let tmp = fresh_test_dir("rejects-plan-done-while-task-incomplete");
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Completed task",
      "status": "done",
      "priority": 1
    },
    {
      "id": "T2",
      "title": "Incomplete task",
      "status": "in_progress",
      "priority": 2
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "set_plan_status",
            "status": "done",
            "rationale": "Exercise plan/task convergence guard"
        });

        let err = handle_plan_action("solo", &tmp, &action)
            .unwrap_err()
            .to_string();

        assert!(err.contains("plan status cannot be set to done while tasks remain incomplete"));
        let persisted = std::fs::read_to_string(tmp.join(MASTER_PLAN_FILE)).unwrap();
        assert!(persisted.contains("\"status\": \"in_progress\""));
    }

    #[test]
    fn plan_set_task_status_marks_only_target_task_done() {
        let tmp = fresh_test_dir("set-task-status-only-target-task");
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Task one",
      "status": "in_progress",
      "priority": 1
    },
    {
      "id": "T2",
      "title": "Task two",
      "status": "todo",
      "priority": 2
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "set_task_status",
            "task_id": "T1",
            "status": "done",
            "rationale": "Close only one task"
        });

        let (_done, out) = handle_plan_action("solo", &tmp, &action).unwrap();
        assert!(out.contains("plan ok"));

        let persisted = std::fs::read_to_string(tmp.join(MASTER_PLAN_FILE)).unwrap();
        assert!(persisted.contains("\"id\": \"T1\""));
        assert!(persisted.contains("\"status\": \"done\""));
        assert!(persisted.contains("\"id\": \"T2\""));
        assert!(persisted.contains("\"status\": \"todo\""));
        assert!(persisted.contains("\"status\": \"in_progress\""));
    }

    #[test]
    fn plan_set_plan_status_allows_task_id_provenance_field() {
        let tmp = fresh_test_dir("set-plan-status-allows-task-id");
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [{"id":"T1","status":"todo"}],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "set_plan_status",
            "task_id": "T1",
            "status": "in_progress",
            "rationale": "Invalid mixed payload"
        });

        let (_, out) = handle_plan_action("solo", &tmp, &action)
            .expect("set_plan_status should accept provenance task_id");
        assert!(out.contains("plan ok"));
    }

    #[test]
    fn plan_set_task_status_rejects_task_object_field() {
        let tmp = fresh_test_dir("set-task-status-rejects-task-object");
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [{"id":"T1","status":"todo"}],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "set_task_status",
            "task_id": "T1",
            "status": "done",
            "task": {"id":"T1","status":"done"},
            "rationale": "Invalid mixed payload"
        });

        let err = handle_plan_action("solo", &tmp, &action)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not accept task"));
    }

    #[test]
    fn objectives_update_objective_reports_requested_and_compared_ids() {
        let tmp = fresh_test_dir("objective-update-not-found-context");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    },
    {
      "id": "obj_beta",
      "title": "Beta",
      "status": "active",
      "scope": "beta scope",
      "authority_files": ["src/objectives.rs"],
      "category": "quality",
      "level": "low",
      "description": "beta",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "update_objective",
            "objective_id": "obj_missing",
            "updates": {
                "scope": "updated"
            }
        });

        let err = handle_objectives_action(&tmp, &action)
            .unwrap_err()
            .to_string();

        assert!(err.contains("requested_raw=\"obj_missing\""));
        assert!(err.contains("objective not found:"));
        assert!(err.contains("requested_id=obj_missing"));
        assert!(err.contains("compared_ids=[\"obj_alpha\", \"obj_beta\"]"));
        assert!(err.contains("compared_normalized_ids=[\"obj_alpha\", \"obj_beta\"]"));
    }

    #[test]
    fn objectives_set_status_matches_normalized_id() {
        let tmp = fresh_test_dir("objective-set-status-normalized-id");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "set_status",
            "objective_id": "`obj_alpha`",
            "status": "done"
        });

        let (_done, out) = handle_objectives_action(&tmp, &action).unwrap();

        assert!(out.contains("objectives set_status ok"));
        let persisted =
            std::fs::read_to_string(tmp.join("agent_state").join("OBJECTIVES.json")).unwrap();
        assert!(persisted.contains("\"status\": \"done\""));
    }

    #[test]
    fn objectives_update_objective_reports_raw_and_normalized_lookup_context() {
        let tmp = fresh_test_dir("objective-update-raw-and-normalized-context");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "update_objective",
            "objective_id": "`obj_missing`",
            "updates": {
                "scope": "updated"
            }
        });

        let err = handle_objectives_action(&tmp, &action)
            .unwrap_err()
            .to_string();

        assert!(err.contains("requested_raw=\"`obj_missing`\""));
        assert!(err.contains("requested_id=obj_missing"));
        assert!(err.contains("compared_ids=[\"obj_alpha\"]"));
        assert!(err.contains("compared_normalized_ids=[\"obj_alpha\"]"));
    }

    #[test]
    fn objectives_create_objective_reports_raw_and_normalized_duplicate_context() {
        let tmp = fresh_test_dir("objective-create-duplicate-context");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "create_objective",
            "objective": {
                "id": "`obj_alpha`",
                "title": "Alpha duplicate",
                "status": "active",
                "scope": "duplicate scope",
                "authority_files": ["src/tools.rs"],
                "category": "quality",
                "level": "low",
                "description": "duplicate",
                "requirement": [],
                "verification": [],
                "success_criteria": []
            }
        });

        let err = handle_objectives_action(&tmp, &action)
            .unwrap_err()
            .to_string();

        assert!(err.contains("objective id already exists:"));
        assert!(err.contains("requested_raw="));
        assert!(err.contains("requested_id=obj_alpha"));
        assert!(err.contains("compared_ids=[\"obj_alpha\"]"));
        assert!(err.contains("compared_normalized_ids=[\"obj_alpha\"]"));
    }

    #[test]
    fn objectives_create_update_read_lifecycle_succeeds() {
        let tmp = fresh_test_dir("objective-create-update-read-lifecycle");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let create_action = json!({
            "op": "create_objective",
            "objective": {
                "id": "obj_lifecycle",
                "title": "Lifecycle",
                "status": "active",
                "scope": "objective lifecycle coverage",
                "authority_files": ["src/tools.rs", "agent_state/OBJECTIVES.json"],
                "category": "quality",
                "level": "medium",
                "description": "create/update/read lifecycle objective",
                "requirement": ["create succeeds"],
                "verification": [],
                "success_criteria": ["updated objective is readable"]
            }
        });
        let (_done, create_out) = handle_objectives_action(&tmp, &create_action).unwrap();
        assert!(create_out.contains("objectives create_objective ok"));

        let update_action = json!({
            "op": "update_objective",
            "objective_id": "obj_lifecycle",
            "updates": {
                "scope": "updated lifecycle scope",
                "description": "updated lifecycle objective",
                "verification": ["updated through handle_objectives_action"]
            }
        });
        let (_done, update_out) = handle_objectives_action(&tmp, &update_action).unwrap();
        assert!(update_out.contains("objectives update_objective ok"));

        let read_action = json!({ "op": "read", "include_done": true });
        let (_done, read_out) = handle_objectives_action(&tmp, &read_action).unwrap();
        assert!(read_out.contains("\"id\": \"obj_lifecycle\""));
        assert!(read_out.contains("\"scope\": \"updated lifecycle scope\""));
        assert!(read_out.contains("\"description\": \"updated lifecycle objective\""));
        assert!(read_out.contains("updated through handle_objectives_action"));
    }

    #[test]
    fn objectives_replace_alias_writes_objectives_atomically() {
        let tmp = fresh_test_dir("objective-replace-alias");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "replace",
            "objectives": {
                "version": 1,
                "objectives": [
                    {
                        "id": "obj_runtime_authority",
                        "title": "Restore runtime objective authority",
                        "status": "active",
                        "scope": "repair runtime authority",
                        "authority_files": ["agent_state/OBJECTIVES.json"],
                        "category": "correctness",
                        "level": "high",
                        "description": "restored from alias replace",
                        "requirement": [],
                        "verification": [],
                        "success_criteria": []
                    }
                ],
                "goal": [],
                "instrumentation": [],
                "definition_of_done": [],
                "non_goals": []
            }
        });

        let (_done, out) = handle_objectives_action(&tmp, &action).unwrap();
        assert!(out.contains("objectives replace_objectives ok"));

        let persisted =
            std::fs::read_to_string(tmp.join("agent_state").join("OBJECTIVES.json")).unwrap();
        assert!(persisted.contains("obj_runtime_authority"));
        let parsed: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        assert_eq!(
            parsed["objectives"][0]["id"].as_str(),
            Some("obj_runtime_authority")
        );

        let tlog = std::fs::read_to_string(tmp.join("agent_state").join("tlog.ndjson")).unwrap();
        assert!(tlog.contains("workspace_artifact_write_requested"));
        assert!(tlog.contains("workspace_artifact_write_applied"));
        assert!(tlog.contains("agent_state/OBJECTIVES.json"));
    }

    #[test]
    fn objectives_update_objective_emits_attempt_and_success_trace_records() {
        let tmp = fresh_test_dir("objective-update-trace-records");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let log_prefix = format!(
            "objective-update-trace-{}",
            fresh_test_dir("trace-log-prefix").display()
        );
        init_log_paths(&log_prefix);
        let action_log = crate::logging::current_action_log_path_for_tests()
            .expect("action log path after init");
        let before_count = std::fs::read_to_string(&action_log)
            .ok()
            .map(|raw| raw.lines().filter(|line| !line.trim().is_empty()).count())
            .unwrap_or(0);

        let action = json!({
            "op": "update_objective",
            "objective_id": "obj_alpha",
            "updates": {
                "scope": "updated alpha scope"
            }
        });

        let (_done, out) = handle_objectives_action(&tmp, &action).unwrap();
        assert!(out.contains("objectives update_objective ok"));

        let raw = std::fs::read_to_string(&action_log).expect("read action log after update");
        let records: Vec<Value> = raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let new_records = &records[before_count..];
        let objective_records: Vec<&Value> = new_records
            .iter()
            .filter(|record| {
                record.get("kind").and_then(|v| v.as_str()) == Some("orch")
                    && record.get("phase").and_then(|v| v.as_str())
                        == Some("objective_operation_context")
            })
            .collect();

        let matching_records: Vec<&Value> = objective_records
            .iter()
            .copied()
            .filter(|record| {
                let meta = record.get("meta");
                meta.and_then(|meta| meta.get("operation"))
                    .and_then(|v| v.as_str())
                    == Some("update_objective")
                    && meta
                        .and_then(|meta| meta.get("requested_id"))
                        .and_then(|v| v.as_str())
                        == Some("obj_alpha")
            })
            .collect();
        let attempt = matching_records
            .iter()
            .rev()
            .copied()
            .find(|record| {
                record
                    .get("meta")
                    .and_then(|meta| meta.get("outcome"))
                    .and_then(|v| v.as_str())
                    == Some("attempt")
            })
            .expect("latest attempt record");
        assert!(attempt.get("text").is_none());
        let attempt_meta = attempt.get("meta").expect("attempt meta");
        assert_eq!(
            attempt_meta.get("operation").and_then(|v| v.as_str()),
            Some("update_objective")
        );
        assert_eq!(
            attempt_meta.get("outcome").and_then(|v| v.as_str()),
            Some("attempt")
        );
        assert_eq!(
            attempt_meta.get("requested_raw").and_then(|v| v.as_str()),
            Some("obj_alpha")
        );
        assert_eq!(
            attempt_meta.get("requested_id").and_then(|v| v.as_str()),
            Some("obj_alpha")
        );
        assert_eq!(
            attempt_meta.get("compared_ids"),
            Some(&json!(["obj_alpha"]))
        );
        assert_eq!(
            attempt_meta.get("compared_normalized_ids"),
            Some(&json!(["obj_alpha"]))
        );

        let success = matching_records
            .iter()
            .rev()
            .copied()
            .find(|record| {
                record
                    .get("meta")
                    .and_then(|meta| meta.get("outcome"))
                    .and_then(|v| v.as_str())
                    == Some("success")
            })
            .expect("latest success record");
        assert!(success.get("text").is_none());
        let success_meta = success.get("meta").expect("success meta");
        assert_eq!(
            success_meta.get("operation").and_then(|v| v.as_str()),
            Some("update_objective")
        );
        assert_eq!(
            success_meta.get("outcome").and_then(|v| v.as_str()),
            Some("success")
        );
        assert_eq!(
            success_meta.get("requested_raw").and_then(|v| v.as_str()),
            Some("obj_alpha")
        );
        assert_eq!(
            success_meta.get("requested_id").and_then(|v| v.as_str()),
            Some("obj_alpha")
        );
        assert_eq!(
            success_meta.get("compared_ids"),
            Some(&json!(["obj_alpha"]))
        );
        assert_eq!(
            success_meta.get("compared_normalized_ids"),
            Some(&json!(["obj_alpha"]))
        );

        let persisted =
            std::fs::read_to_string(tmp.join("agent_state").join("OBJECTIVES.json")).unwrap();
        assert!(persisted.contains("\"scope\": \"updated alpha scope\""));

        let last_objective_record = objective_records
            .last()
            .copied()
            .expect("at least one objective trace record");
        assert_eq!(
            last_objective_record.get("phase").and_then(|v| v.as_str()),
            Some("objective_operation_context")
        );
    }
}
