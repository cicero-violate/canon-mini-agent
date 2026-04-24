/// Intent: canonical_write
/// Resource: error
/// Inputs: &str, &str
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
    let derived = crate::prompt_inputs::render_diagnostics_report_from_issues(workspace);
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &str, usize, &std::path::Path, &[(std::string::String, std::option::Option<std::string::String>
/// Outputs: ()
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: usize, &str, &str, &str, std::option::Option<&str>, &str
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &str, std::option::Option<&str>, std::option::Option<&str>, std::option::Option<&str>, std::option::Option<&str>
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &str, usize, &std::path::Path, &str
/// Outputs: std::option::Option<(bool, std::string::String)>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: event_append
/// Resource: error
/// Inputs: &mut std::string::String, &str, &std::path::Path
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &str
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &str
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &semantic::SemanticIndex, &std::path::Path, &str, std::option::Option<&str>, std::option::Option<&str>
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &semantic::SemanticIndex, &std::path::Path, &str, std::option::Option<&str>, std::option::Option<&str>
/// Outputs: std::result::Result<std::string::String, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &semantic::SemanticIndex, &std::path::Path, &str, &str
/// Outputs: std::result::Result<std::string::String, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &semantic::SemanticIndex, &std::path::Path, &str, std::option::Option<&str>
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &str, &str
/// Outputs: std::result::Result<std::string::String, anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<(&str, std::path::PathBuf), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &std::path::Path, &str, bool, &str, &str
/// Outputs: std::result::Result<std::string::String, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &str
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &std::path::Path, &str, &str, std::option::Option<&std::path::PathBuf>
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::path::Path, &std::path::Path, &str
/// Outputs: std::result::Result<(std::string::String, std::option::Option<std::path::PathBuf>), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &str, &str
/// Outputs: std::result::Result<std::path::PathBuf, anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<(std::string::String, std::option::Option<std::string::String>, std::path::PathBuf), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, std::option::Option<&str>
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
    let payload = graph_reports_failure_payload(
        action_kind,
        crate_name,
        artifact_crate,
        cmd,
        out_dir_str,
        tlog,
    );
    log_error_event(
        role,
        action_kind,
        Some(step),
        &graph_reports_failure_message(action_kind, crate_name),
        Some(payload),
    );
}

fn graph_reports_failure_message(action_kind: &str, crate_name: &str) -> String {
    format!("{action_kind} failed for crate {crate_name}")
}

fn graph_reports_failure_payload(
    action_kind: &str,
    crate_name: &str,
    artifact_crate: &str,
    cmd: &str,
    out_dir_str: &str,
    tlog: Option<&str>,
) -> serde_json::Value {
    json!({
        "stage": action_kind,
        "crate": crate_name,
        "artifact_crate": artifact_crate,
        "cmd": cmd,
        "out_dir": out_dir_str.to_string(),
        "tlog": tlog,
    })
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &std::path::Path, &str
/// Outputs: std::result::Result<std::string::String, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &str, &std::path::Path, &str
/// Outputs: std::result::Result<std::string::String, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, std::option::Option<&str>
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_cargo_test_command(crate_name: &str, test_name: Option<&str>) -> String {
    if let Some(test_name) = test_name {
        format!("cargo test -q -p {} {} -- --exact", crate_name, test_name)
    } else {
        // Faster default profile: skip doc tests and suppress noisy output.
        // Callers can still target explicit tests via `test`.
        format!("cargo test -q -p {} --lib --bins --tests", crate_name)
    }
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::vec::Vec<std::string::String>
/// Effects: fs_read, state_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &[std::string::String]
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::option::Option<std::string::String>
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: event_append
/// Resource: error
/// Inputs: &mut std::string::String, &serde_json::Value, &str, &str, bool
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &serde_json::Value, std::option::Option<&std::path::Path>, std::option::Option<&str>
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_write
/// Resource: error
/// Inputs: &str, usize, &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: fs_write, state_write, transitions_state
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

