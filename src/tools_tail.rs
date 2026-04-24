/// Write the canonical stage graph artifact to `agent_state/orchestrator/stage_graph.json`.
/// Called automatically at agent-loop startup so the file is always present as a live artifact.
pub(crate) fn write_stage_graph(workspace: &Path) {
    if let Err(e) = write_stage_graph_inner(workspace, "agent_state/orchestrator/stage_graph.json")
    {
        eprintln!("[stage_graph] failed to write live artifact: {e}");
    }
}

/// Intent: canonical_write
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

/// Intent: pure_transform
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

/// Intent: pure_transform
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

/// Intent: pure_transform
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
        "invariants" => crate::invariants::handle_invariants_action(workspace, item),
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

/// Intent: event_append
fn append_rejected_batch_item(out: &mut String, n: usize, total: usize, kind: &str, item: &Value) {
    let op_note = batch_item_op_note(item);
    out.push_str(&format_rejected_batch_item(n, total, kind, &op_note));
}

/// Intent: pure_transform
fn format_rejected_batch_item(n: usize, total: usize, kind: &str, op_note: &str) -> String {
    format!(
        "[batch {n}/{total}: REJECTED {kind}{op_note}]\n\
         mutating action '{kind}{op_note}' is not allowed in batch\n\n"
    )
}

/// Intent: event_append
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

/// Intent: event_append
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

/// Intent: canonical_write
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

/// Intent: pure_transform
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
