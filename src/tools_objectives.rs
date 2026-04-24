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

/// Intent: pure_transform
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

/// Intent: pure_transform
fn parse_objectives_file_strict(raw: &str) -> Result<crate::objectives::ObjectivesFile> {
    let file: crate::objectives::ObjectivesFile =
        serde_json::from_str(raw).map_err(|e| anyhow!("failed to parse OBJECTIVES.json: {e}"))?;
    validate_unique_objective_ids(&file)?;
    Ok(file)
}

/// Intent: pure_transform
fn parse_objectives_file_or_default(raw: &str) -> crate::objectives::ObjectivesFile {
    serde_json::from_str(raw).unwrap_or_default()
}

/// Intent: validation_gate
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

/// Intent: canonical_write
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
            hash: crate::logging::stable_hash_hex(&contents),
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
    let objective_id = normalize_objective_id_for_match(objective_id);
    if objective_id.is_empty() {
        bail!("objectives update_objective missing objective_id");
    }
    let updates = action
        .get("updates")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("objectives update_objective missing updates object. Required schema: {{\"op\":\"update_objective\",\"objective_id\":\"<id>\",\"updates\":{{\"title\":\"<title>\",\"status\":\"<status>\"}}}}"))?;
    let mut file = parse_objectives_file_strict(raw)?;
    log_objective_operation_context(
        "update_objective",
        "attempt",
        Some(&objective_id),
        &file.objectives,
    );
    let mut found = false;
    for obj in file.objectives.iter_mut() {
        if objective_id_matches(&obj.id, &objective_id) {
            apply_objective_updates(obj, updates)?;
            found = true;
            break;
        }
    }
    if !found {
        // Planner sometimes emits update/set_status for objectives that are not
        // yet materialized in OBJECTIVES.json. Auto-create a stub to prevent
        // missing-target blocker loops.
        let mut created = synthesize_objective_stub(&objective_id, None, Some(updates));
        apply_objective_updates(&mut created, updates)?;
        file.objectives.push(created);
        log_objective_operation_context(
            "update_objective",
            "auto_created",
            Some(&objective_id),
            &file.objectives,
        );
        return write_objectives_file(workspace, path, &file, writer)
            .map(|_| (false, "objectives update_objective ok (auto-created)".to_string()));
    }
    log_objective_operation_context(
        "update_objective",
        "success",
        Some(&objective_id),
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
    let objective_id = normalize_objective_id_for_match(objective_id);
    if objective_id.is_empty() {
        bail!("objectives set_status missing objective_id");
    }
    let status = action
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("objectives set_status missing status"))?;
    let mut file = parse_objectives_file_or_default(raw);
    log_objective_operation_context(
        "set_status",
        "attempt",
        Some(&objective_id),
        &file.objectives,
    );
    let mut found = false;
    for obj in file.objectives.iter_mut() {
        if objective_id_matches(&obj.id, &objective_id) {
            obj.status = status.to_string();
            found = true;
            break;
        }
    }
    if !found {
        // Missing objective IDs are treated as materialization lag, not hard
        // failure. Create a minimal objective and apply requested status.
        file.objectives
            .push(synthesize_objective_stub(&objective_id, Some(status), None));
        log_objective_operation_context(
            "set_status",
            "auto_created",
            Some(&objective_id),
            &file.objectives,
        );
        return write_objectives_file(workspace, path, &file, writer)
            .map(|_| (false, "objectives set_status ok (auto-created)".to_string()));
    }
    log_objective_operation_context(
        "set_status",
        "success",
        Some(&objective_id),
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

fn apply_objective_updates(
    objective: &mut crate::objectives::Objective,
    updates: &serde_json::Map<String, Value>,
) -> Result<()> {
    let mut value = serde_json::to_value(objective.clone())?;
    if let Some(map) = value.as_object_mut() {
        for (k, v) in updates {
            map.insert(k.clone(), v.clone());
        }
    }
    *objective = serde_json::from_value(value)?;
    Ok(())
}

fn synthesize_objective_stub(
    objective_id: &str,
    status: Option<&str>,
    updates: Option<&serde_json::Map<String, Value>>,
) -> crate::objectives::Objective {
    let update_string = |key: &str| {
        updates
            .and_then(|m| m.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let update_string_vec = |key: &str| {
        updates
            .and_then(|m| m.get(key))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|items| !items.is_empty())
            .unwrap_or_default()
    };
    let normalized_status = status
        .map(str::to_string)
        .or_else(|| update_string("status"))
        .unwrap_or_else(|| "active".to_string());
    let title = update_string("title").unwrap_or_else(|| humanize_objective_id(objective_id));
    let scope = update_string("scope").unwrap_or_else(|| "auto-generated".to_string());
    let category = update_string("category").unwrap_or_else(|| "maintenance".to_string());
    let level = update_string("level").unwrap_or_else(|| "low".to_string());
    let authority_files = update_string_vec("authority_files");
    let description = update_string("description").unwrap_or_else(|| {
        format!(
            "Auto-created objective stub for missing objective id `{objective_id}` so planning can proceed without missing-target loops."
        )
    });
    crate::objectives::Objective {
        id: objective_id.to_string(),
        title,
        status: normalized_status,
        scope,
        authority_files,
        category,
        level,
        description,
        ..crate::objectives::Objective::default()
    }
}

fn humanize_objective_id(objective_id: &str) -> String {
    let trimmed = objective_id.trim();
    let core = trimmed
        .strip_prefix("obj_")
        .or_else(|| trimmed.strip_prefix("obj-"))
        .unwrap_or(trimmed);
    let mut out = String::new();
    for (idx, part) in core
        .split(|ch: char| ch == '_' || ch == '-' || ch.is_whitespace())
        .filter(|part| !part.is_empty())
        .enumerate()
    {
        if idx > 0 {
            out.push(' ');
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() {
        objective_id.to_string()
    } else {
        out
    }
}
