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

/// Intent: repair_or_initialize
/// Resource: reopened_task_regression_linkage
/// Inputs: &serde_json::Map<std::string::String, serde_json::Value>, &serde_json::Map<std::string::String, serde_json::Value>, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: none
/// Forbidden: accepting reopened done tasks without regression-test linkage
/// Invariants: status changes from done to non-done must include regression-test linkage in steps
/// Failure: bails when reopened task lacks required regression linkage
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: plan_action_shape
/// Inputs: &serde_json::Value, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: none
/// Forbidden: mutation
/// Invariants: dispatches known plan operations to their shape validators and accepts view/update/unknown operations without shape checks
/// Failure: returns delegated validation errors
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &serde_json::Value, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &serde_json::Value, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: plan_delete_task_shape
/// Inputs: &serde_json::Value, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: none
/// Forbidden: mutation
/// Invariants: delete_task requires task_id and rejects task, status, edge, and full-plan fields
/// Failure: returns missing-field or rejected-field validation errors
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &serde_json::Value, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: plan_status_shape
/// Inputs: &serde_json::Value, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: none
/// Forbidden: mutation
/// Invariants: set_plan_status requires status and rejects task, edge, and full-plan fields
/// Failure: returns missing-field or rejected-field validation errors
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &serde_json::Value, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &serde_json::Value, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: transport_effect
/// Resource: plan_dispatch
/// Inputs: tools::PlanOp, &mut serde_json::Map<std::string::String, serde_json::Value>, &serde_json::Value
/// Outputs: std::result::Result<std::option::Option<(bool, std::string::String)>, anyhow::Error>
/// Effects: mutates plan object according to supported plan operation handlers
/// Forbidden: replace_plan dispatch after object mutation path
/// Invariants: replace_plan is handled before dispatch; add_edge may return early with handler result; all other operations return None on success
/// Failure: returns handler validation or mutation errors
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_write
/// Resource: plan_action_update
/// Inputs: &str, &serde_json::Value, &str, &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: writes PLAN projection and appends control-plane plan update log
/// Forbidden: mutation outside canonical plan projection/logging paths
/// Invariants: persists pretty PLAN JSON with op-specific signature and evaluates ready-task side effect after write
/// Failure: returns projection write or JSON serialization errors
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_write
/// Resource: PLAN.json + tlog.ndjson
/// Inputs: &std::path::Path, &serde_json::Value, &str, &std::path::Path, &serde_json::Value, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: writes canonical PLAN projection and appends plan_update control log
/// Forbidden: direct non-canonical PLAN mutation
/// Invariants: projection write precedes control log append; ready-task side effect is derived from the accepted plan op
/// Failure: returns write/serialization errors; logging side effect is best-effort
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: replacement_plan
/// Inputs: &serde_json::Value
/// Outputs: std::result::Result<serde_json::Value, anyhow::Error>
/// Effects: none
/// Forbidden: mutation of input action
/// Invariants: replacement plan is normalized and must contain tasks plus dag.edges forming a valid DAG
/// Failure: returns errors for missing plan fields, normalization failures, or invalid DAG structure
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_read
/// Resource: error
/// Inputs: &serde_json::Map<std::string::String, serde_json::Value>
/// Outputs: std::result::Result<&std::vec::Vec<serde_json::Value>, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn get_tasks_array(obj: &serde_json::Map<String, Value>) -> Result<&Vec<Value>> {
    obj.get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value
/// Outputs: std::result::Result<(&str, &str), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: plan_edge_ids
/// Inputs: &std::collections::BTreeSet<std::string::String>, &str, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: none
/// Forbidden: mutation
/// Invariants: both edge endpoints must exist in known task id set
/// Failure: returns unknown-task-id validation error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value
/// Outputs: &str
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &str, &serde_json::Value, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &str, &serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
    for (key, value) in task_obj.iter().filter(|(key, _)| key.as_str() != "id") {
        existing.insert(key.to_string(), value.clone());
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
        let existing = tasks_obj
            .iter_mut()
            .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(id))
            .and_then(|t| t.as_object_mut())
            .ok_or_else(|| anyhow!("plan task not found: {id}"))?;
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

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<serde_json::Value, anyhow::Error>
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: plan_object
/// Inputs: &mut serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: normalizes PLAN.json object in place
/// Forbidden: mutation outside provided plan value
/// Invariants: ensures version >= 2, default status, tasks array, and dag.edges array
/// Failure: returns error when plan or dag is not a JSON object
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: repair_or_initialize
/// Resource: plan_dag
/// Inputs: &[serde_json::Value], &[serde_json::Value]
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: none
/// Forbidden: mutation
/// Invariants: edges must have non-empty from/to task ids, reference known tasks, and form an acyclic graph
/// Failure: returns validation errors for malformed edges, unknown task ids, or detected cycles
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: transport_effect
/// Resource: error
/// Inputs: &str, &std::path::Path
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: spawns_process
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: transport_effect
/// Resource: error
/// Inputs: &str, &std::path::Path
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: spawns_process
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: event_append
/// Resource: error
/// Inputs: &mut std::string::String, &str
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: transport_effect
/// Resource: error
/// Inputs: &str, &std::path::Path
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: fs_write, spawns_process
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: transport_effect
/// Resource: error
/// Inputs: &std::path::Path, &str, &str, u64
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: spawns_process
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: transport_effect
/// Resource: error
/// Inputs: &std::path::Path, &str, &str
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: fs_write, spawns_process
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: repair_or_initialize
/// Resource: error
/// Inputs: &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &str, &semantic::ExecutionPathPlan
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &str, &semantic::ExecutionPathPlan
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn persist_rebound_execution_plan(
    workspace: &Path,
    crate_name: &str,
    plan: &crate::semantic::ExecutionPathPlan,
) -> Result<()> {
    let out_path = execution_plan_rebound_path(workspace, crate_name);
    fs::write(&out_path, serde_json::to_vec_pretty(plan)?)
        .with_context(|| format!("write {}", out_path.display()))
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path, &str
/// Outputs: std::option::Option<semantic::ExecutionPathPlan>
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path, &str
/// Outputs: tools::LearningBiasStats
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
        .and_then(crate::semantic::build_apply_patch_template);
}

/// Intent: event_append
/// Resource: error
/// Inputs: &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::option::Option<(std::string::String, u32, u32)>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_read
/// Resource: semantic_index
/// Inputs: &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<semantic::SemanticIndex, anyhow::Error>
/// Effects: reads semantic graph index from workspace state
/// Forbidden: mutation
/// Invariants: resolves crate name from action and loads matching SemanticIndex
/// Failure: returns contextual error when semantic index is unavailable
/// Provenance: rustc:facts + rustc:docstring
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
