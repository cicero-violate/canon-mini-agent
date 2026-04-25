// ── Agent loop ─────────────────────────────────────────────────────────────────

/// Run one agent role until it calls `message` with status=complete or exhausts MAX_STEPS.
/// Returns the typed completion on success, or an error on hard failure.
/// `check_on_done`: if true, run cargo build + test before accepting completion.
async fn run_agent(
    role: &str,
    prompt_kind: &str,
    system_instructions: &str,
    initial_prompt: String,
    endpoint: &LlmEndpoint,
    bridge: &WsBridge,
    workspace: &Path,
    tabs: &TabManagerHandle,
    writer: Option<&mut CanonicalWriter>,
    effect_tx: Option<UnboundedSender<EffectEvent>>,
    submit_only: bool,
    check_on_done: bool,
    send_system_prompt: bool,
    initial_steps_used: usize,
) -> Result<AgentCompletion> {
    eprintln!(
        "[{role}] endpoint_id={} url={} prompt_kind={} submit_only={}",
        endpoint.id,
        endpoint.pick_url(0),
        prompt_kind,
        submit_only
    );
    let mut step = 0usize;
    let mut last_result: Option<String> = None;
    let mut last_tab_id: Option<u32> = None;
    let mut last_turn_id: Option<u64> = None;
    let mut last_action: Option<String> = None;
    let mut last_provenance = ActionProvenance::default();
    let mut last_predicted_next_actions: Option<String> = None;
    let mut error_streak: usize = 0;
    #[allow(unused_assignments)]
    let mut last_error: Option<String> = None;
    let mut reaction_only_streak: usize = 0;
    let mut cargo_test_gate = CargoTestGate::new();
    let task_context = initial_prompt.clone();
    let mut idle_streak = 0usize;
    let mut repeated_failed_action_fingerprint: Option<String> = None;
    let mut last_failed_action_fingerprint: Option<String> = None;
    let mut repeated_failed_action_count: usize = 0;
    let mut last_semantic_action_fingerprint: Option<String> = None;
    let mut repeated_semantic_action_count: usize = 0;
    let mut transient_service_retry_streak: usize = 0;
    let shutdown = shutdown_signal();
    let mut ctx = LlmResponseContext {
        role,
        endpoint,
        prompt_kind,
        submit_only,
        writer,
        effect_tx,
    };

    write_stage_graph(workspace);
    write_tool_examples(workspace);

    loop {
        if let Some(sig) = shutdown.as_ref() {
            if sig.flag.load(Ordering::SeqCst) {
                return Ok(AgentCompletion::Summary("shutdown requested".to_string()));
            }
        }
        if step >= MAX_STEPS {
            bail!("[{role}] exhausted {MAX_STEPS} steps without completing");
        }

        let total_steps = if role.starts_with("executor") {
            initial_steps_used.saturating_add(step)
        } else {
            step
        };

        if role.starts_with("planner")
            && executor_step_limit_exceeded(total_steps, crate::constants::PLANNER_STEP_LIMIT)
        {
            last_result = Some(planner_step_limit_feedback());
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                role,
                "step_limit",
                &format!(
                    "planner reached step limit ({})",
                    crate::constants::PLANNER_STEP_LIMIT
                ),
                None,
            );
        } else if role.starts_with("executor")
            && executor_step_limit_exceeded(total_steps, EXECUTOR_STEP_LIMIT)
        {
            last_result = Some(executor_step_limit_feedback());
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                role,
                "step_limit",
                &format!("executor reached step limit ({EXECUTOR_STEP_LIMIT})"),
                None,
            );
        }

        let send_system_this_turn =
            should_send_system_prompt(send_system_prompt, endpoint.stateful, step);
        let (role_schema, prompt) = build_agent_prompt(
            role,
            send_system_this_turn,
            step,
            &initial_prompt,
            system_instructions,
            last_result.as_deref(),
            last_tab_id,
            last_turn_id,
            last_action.as_deref(),
            &last_provenance,
            total_steps,
            last_predicted_next_actions.as_deref(),
        );
        let exchange_id = make_command_id(role, prompt_kind, step + 1);

        eprintln!("[{role}] step={} prompt_bytes={}", step + 1, prompt.len());
        crate::logging::record_prompt_overflow(workspace, role, prompt.len());
        ctx.log_request(step + 1, &exchange_id, &prompt, &role_schema);

        let response_timeout_secs = response_timeout_for_role(role);
        let request_future = llm_worker_send_request_with_req_id_timeout(
            bridge,
            &endpoint.id,
            &endpoint.url,
            endpoint.stateful,
            &prompt,
            &role_schema,
            None,
            None,
            false,
            true,
            role,
            tabs,
            endpoint.max_tabs,
            submit_only,
            Some(response_timeout_secs),
        );
        let req_result = match shutdown.as_ref() {
            Some(sig) => {
                tokio::select! {
                    res = request_future => res,
                    _ = sig.notify.notified() => {
                        return Ok(AgentCompletion::Summary("shutdown requested".to_string()));
                    }
                }
            }
            None => request_future.await,
        };
        let (_req_id, resp) = match req_result {
            Ok(r) => r,
            Err(e) => {
                let err_text = e.to_string();
                eprintln!("[{role}] step={} llm_error: {e}", step + 1);
                ctx.log_error(step + 1, &exchange_id, &err_text);
                ctx.record_effect(crate::events::EffectEvent::LlmErrorBoundary {
                    role: role.to_string(),
                    prompt_kind: prompt_kind.to_string(),
                    step: step + 1,
                    endpoint_id: endpoint.id.clone(),
                    exchange_id: exchange_id.clone(),
                    error: err_text.clone(),
                });
                crate::blockers::record_action_failure_with_writer(
                    workspace,
                    None,
                    role,
                    "llm_request",
                    &err_text,
                    None,
                );
                if is_chromium_transport_error(&err_text) {
                    apply_error_result(
                        role,
                        &task_context,
                        &mut error_streak,
                        &mut last_error,
                        &mut last_result,
                        &err_text,
                        format!(
                            "Chromium transport failure recovered locally. Retry on the same endpoint.\n\
                             Do not emit a blocker handoff for this transport error unless retries are exhausted.\n\
                             {}\n{}\n\nTask context:\n{}",
                            crate::prompt_contract::ACTION_EMIT_LINE,
                            crate::prompt_contract::OUTPUT_FORMAT_LINE,
                            truncate(&task_context, MAX_SNIPPET)
                        ),
                    );
                    step += 1;
                    continue;
                }
                apply_error_result(
                    role,
                    &task_context,
                    &mut error_streak,
                    &mut last_error,
                    &mut last_result,
                    &err_text,
                    format!(
                        "LLM error: {e}\n{}\n{}\n\nTask context:\n{}",
                        crate::prompt_contract::ACTION_EMIT_LINE,
                        crate::prompt_contract::OUTPUT_FORMAT_LINE,
                        truncate(&task_context, MAX_SNIPPET)
                    ),
                );
                step += 1;
                continue;
            }
        };
        let tab_id = resp.tab_id;
        let turn_id = resp.turn_id;
        last_tab_id = tab_id;
        last_turn_id = turn_id;
        let raw = resp.raw;

        ctx.log_response(step + 1, &exchange_id, &raw, tab_id, turn_id);

        if let Some(ack) = ctx.handle_submit_ack(step + 1, &exchange_id, &raw) {
            return Ok(AgentCompletion::Summary(ack));
        }

        eprintln!("[{role}] step={} response_bytes={}", step + 1, raw.len());

        if is_transient_service_response(&raw) {
            transient_service_retry_streak = transient_service_retry_streak.saturating_add(1);
            log_message_event(
                role,
                endpoint,
                prompt_kind,
                step + 1,
                &exchange_id,
                "llm_transient_service_retry",
                json!({
                    "raw": truncate(&raw, MAX_SNIPPET),
                    "retry_streak": transient_service_retry_streak,
                }),
            );
            if transient_service_retry_streak <= 3 {
                eprintln!(
                    "[{role}] step={} transient_service_response retry {}",
                    step + 1,
                    transient_service_retry_streak
                );
                continue;
            }
        } else {
            transient_service_retry_streak = 0;
        }

        if ctx.handle_reaction_only(
            step + 1,
            &exchange_id,
            &raw,
            &mut reaction_only_streak,
            &mut error_streak,
            &mut last_error,
        ) {
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                role,
                "reaction_only",
                "LLM returned prose without a JSON action block",
                None,
            );
            if !should_force_blocker(reaction_only_streak) {
                continue;
            }
            reaction_only_streak = 0;
            apply_error_result(
                role,
                &task_context,
                &mut error_streak,
                &mut last_error,
                &mut last_result,
                "reaction_only_response",
                build_invalid_action_feedback(None, "reaction-only response", role),
            );
            step += 1;
            continue;
        }

        let mut action = match parse_action_from_raw(
            role,
            endpoint,
            prompt_kind,
            step + 1,
            &exchange_id,
            &raw,
            false,
            true, // always auto-fill message fields so `from` is forced to the actual role
            None,
        ) {
            Ok(action) => action,
            Err(invalid) => {
                crate::blockers::record_action_failure_with_writer(
                    workspace,
                    None,
                    role,
                    "schema_validation",
                    &invalid.err_text,
                    None,
                );
                apply_error_result(
                    role,
                    &task_context,
                    &mut error_streak,
                    &mut last_error,
                    &mut last_result,
                    &invalid.err_text,
                    invalid.feedback,
                );
                step += 1;
                continue;
            }
        };

        let kind = action
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        if kind == "run_command" {
            let cmd = action.get("cmd").and_then(|v| v.as_str());
            cargo_test_gate.note_action(&kind, cmd);
        }
        if kind != "message"
            && enforce_executor_step_limit(
                role,
                total_steps,
                &mut error_streak,
                &mut last_result,
                workspace,
            )
        {
            step += 1;
            continue;
        }
        if let Some(msg) =
            cargo_test_gate.message_blocker_if_needed(&kind, crate::constants::workspace())
        {
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                role,
                "build_gate",
                &msg,
                None,
            );
            error_streak = error_streak.saturating_add(1);
            last_result = Some(msg);
            step += 1;
            continue;
        }

        reaction_only_streak = 0;
        transient_service_retry_streak = 0;
        error_streak = 0;
        eprintln!("[{role}] step={} action={}", step + 1, kind);
        last_action = Some(kind.clone());
        last_provenance = ActionProvenance::from_action(&action);
        last_predicted_next_actions = action
            .get("predicted_next_actions")
            .and_then(|v| serde_json::to_string(v).ok());
        append_orchestration_trace(
            "llm_message_processed",
            json!({
                "role": role,
                "prompt_kind": prompt_kind,
                "step": step + 1,
                "endpoint_id": endpoint.id,
                "action": kind,
            }),
        );

        let command_id = exchange_id.clone();
        action["command_id"] = Value::String(command_id.clone());
        let action_fingerprint = action_retry_fingerprint(&action);
        let semantic_fingerprint = semantic_action_fingerprint(&action);

        if repeated_failed_action_fingerprint.as_deref() == Some(action_fingerprint.as_str()) {
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                role,
                "repeated_failed_action",
                &format!(
                    "identical action payload failed repeatedly: {}",
                    &action_fingerprint
                ),
                action.get("task_id").and_then(|v| v.as_str()),
            );
            apply_error_result(
                role,
                &task_context,
                &mut error_streak,
                &mut last_error,
                &mut last_result,
                "repeated_failed_action_payload",
                "This exact action payload has failed repeatedly. Choose a different action class or a materially different payload before retrying.".to_string(),
            );
            step += 1;
            continue;
        }

        if role == "solo" {
            let objectives_text = canonical_objectives_text(workspace);
            let plan_text = read_text_or_empty(workspace.join(MASTER_PLAN_FILE));
            if should_reject_solo_self_complete(&action, &objectives_text, &plan_text) {
                crate::blockers::record_action_failure_with_writer(
                    workspace,
                    None,
                    role,
                    "solo_completion_gate",
                    "solo attempted self-complete with active objectives and no incomplete plan tasks",
                    None,
                );
                apply_error_result(
                    role,
                    &task_context,
                    &mut error_streak,
                    &mut last_error,
                    &mut last_result,
                    "solo_completion_requires_plan_work_for_active_objectives",
                    "Create/update PLAN tasks for active objectives, or mark objectives deferred/blocked with rationale.".to_string(),
                );
                step += 1;
                continue;
            }
        }

        if is_explicit_idle_action(&action) {
            idle_streak += 1;
            if idle_streak >= 3 {
                crate::blockers::record_action_failure_with_writer(
                    workspace,
                    None,
                    role,
                    "idle_streak",
                    "agent stuck: 3 consecutive explicit idle actions with no progress",
                    None,
                );
                bail!("[{role}] stuck: no progress in 3 steps (repeated explicit idle commands)");
            }
        } else {
            idle_streak = 0;
        }

        let step_result = process_action_and_execute(
            role,
            prompt_kind,
            endpoint,
            workspace,
            step + 1,
            &command_id,
            &action,
            check_on_done,
            ctx.writer.as_deref_mut(),
        )?;

        match step_result {
            (true, reason) => {
                eprintln!("[{role}] message complete: {reason}");
                return Ok(
                    if action.get("action").and_then(|v| v.as_str()) == Some("message") {
                        AgentCompletion::MessageAction {
                            action,
                            summary: reason,
                        }
                    } else {
                        AgentCompletion::Summary(reason)
                    },
                );
            }
            (false, out) => {
                cargo_test_gate.note_result(&kind, &out);
                if role.starts_with("planner") && kind == "message" {
                    if last_semantic_action_fingerprint
                        .as_deref()
                        .is_some_and(|fingerprint| fingerprint == semantic_fingerprint)
                    {
                        repeated_semantic_action_count =
                            repeated_semantic_action_count.saturating_add(1);
                    } else {
                        last_semantic_action_fingerprint = Some(semantic_fingerprint.clone());
                        repeated_semantic_action_count = 1;
                    }
                } else {
                    last_semantic_action_fingerprint = None;
                    repeated_semantic_action_count = 0;
                }

                if role.starts_with("planner")
                    && kind == "message"
                    && repeated_semantic_action_count >= 3
                {
                    crate::blockers::record_action_failure_with_writer(
                        workspace,
                        None,
                        role,
                        "semantic_repeat_no_progress",
                        "planner repeated the same message/handoff shape without changing plan or objective state",
                        action.get("task_id").and_then(|v| v.as_str()),
                    );
                    last_result = Some(
                        "Planner loop detected: you repeated the same message/handoff shape without changing plan or objective state. Choose a materially different action: mutate PLAN/objectives from fresh source evidence, or emit a blocker explaining why no structural update is possible.".to_string(),
                    );
                    step += 1;
                    continue;
                }

                if out.starts_with("Error executing action:") {
                    if last_failed_action_fingerprint
                        .as_deref()
                        .is_some_and(|f| f == action_fingerprint)
                    {
                        repeated_failed_action_count =
                            repeated_failed_action_count.saturating_add(1);
                    } else {
                        last_failed_action_fingerprint = Some(action_fingerprint.clone());
                        repeated_failed_action_count = 1;
                    }
                    if repeated_failed_action_count >= 2 {
                        repeated_failed_action_fingerprint = Some(action_fingerprint.clone());
                    }
                } else {
                    last_failed_action_fingerprint = None;
                    repeated_failed_action_count = 0;
                    repeated_failed_action_fingerprint = None;
                }
                // Persist the last action result so it can be re-injected into the
                // initial prompt if the supervisor restarts the process mid-cycle
                // (e.g. after apply_patch triggers a binary rebuild).  The file is
                // consumed once on startup and then deleted.
                let resume = write_post_restart_result(
                    role,
                    kind.as_str(),
                    &out,
                    step + 1,
                    last_tab_id,
                    last_turn_id,
                    &endpoint.id,
                    "process_restart",
                );
                ctx.record_effect(crate::events::EffectEvent::PostRestartResultRecorded {
                    role: resume.role.clone(),
                    action: resume.action.clone(),
                    result: resume.result.clone(),
                    step: resume.step,
                    tab_id: resume.tab_id,
                    turn_id: resume.turn_id,
                    endpoint_id: resume.endpoint_id.clone(),
                    restart_kind: resume.restart_kind.clone(),
                    signature: resume.signature.clone(),
                });
                last_result = Some(out);
                if kind.as_str() == "apply_patch"
                    && last_result
                        .as_deref()
                        .unwrap_or_default()
                        .starts_with("apply_patch ok")
                {
                    return Ok(AgentCompletion::Summary(last_result.unwrap_or_default()));
                }
            }
        }
        step += 1;
    }
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &str, &str, &str, usize, std::option::Option<u32>, std::option::Option<u64>, &str, &str
/// Outputs: app::PostRestartResult
/// Effects: fs_write, state_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn write_post_restart_result(
    role: &str,
    action: &str,
    result: &str,
    step: usize,
    tab_id: Option<u32>,
    turn_id: Option<u64>,
    endpoint_id: &str,
    restart_kind: &str,
) -> PostRestartResult {
    let path =
        std::path::Path::new(crate::constants::agent_state_dir()).join("post_restart_result.json");
    let signature = artifact_signature(&[
        role,
        action,
        &step.to_string(),
        endpoint_id,
        &result.len().to_string(),
    ]);
    let payload = build_post_restart_result_payload(
        role,
        action,
        result,
        step,
        tab_id,
        turn_id,
        endpoint_id,
        restart_kind,
        &signature,
    );
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    );
    PostRestartResult {
        role: role.to_string(),
        action: action.to_string(),
        result: result.to_string(),
        step,
        tab_id,
        turn_id,
        endpoint_id: endpoint_id.to_string(),
        restart_kind: restart_kind.to_string(),
        signature,
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &str, usize, std::option::Option<u32>, std::option::Option<u64>, &str, &str, &str
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_post_restart_result_payload(
    role: &str,
    action: &str,
    result: &str,
    step: usize,
    tab_id: Option<u32>,
    turn_id: Option<u64>,
    endpoint_id: &str,
    restart_kind: &str,
    signature: &str,
) -> serde_json::Value {
    post_restart_result_payload_json(PostRestartResultPayload {
        role,
        action,
        result,
        step,
        tab_id,
        turn_id,
        endpoint_id,
        restart_kind,
        signature,
    })
}

struct PostRestartResultPayload<'a> {
    role: &'a str,
    action: &'a str,
    result: &'a str,
    step: usize,
    tab_id: Option<u32>,
    turn_id: Option<u64>,
    endpoint_id: &'a str,
    restart_kind: &'a str,
    signature: &'a str,
}

fn post_restart_result_payload_json(payload: PostRestartResultPayload<'_>) -> serde_json::Value {
    serde_json::json!({
        "role": payload.role,
        "action": payload.action,
        "result": payload.result,
        "step": payload.step,
        "tab_id": payload.tab_id,
        "turn_id": payload.turn_id,
        "endpoint_id": payload.endpoint_id,
        "restart_kind": payload.restart_kind,
        "signature": payload.signature,
    })
}

#[derive(Clone, Debug)]
struct PostRestartResult {
    role: String,
    action: String,
    result: String,
    step: usize,
    tab_id: Option<u32>,
    turn_id: Option<u64>,
    endpoint_id: String,
    restart_kind: String,
    signature: String,
}

#[derive(Clone, Debug)]
enum AgentCompletion {
    Summary(String),
    MessageAction { action: Value, summary: String },
}

impl AgentCompletion {
    fn summary_text(&self) -> &str {
        match self {
            Self::Summary(summary) => summary,
            Self::MessageAction { summary, .. } => summary,
        }
    }

    fn into_summary(self) -> String {
        match self {
            Self::Summary(summary) => summary,
            Self::MessageAction { summary, .. } => summary,
        }
    }
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &str
/// Outputs: std::option::Option<app::PostRestartResult>
/// Effects: fs_read, logging, state_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring + rustc:effects
fn peek_post_restart_result(role: &str) -> Option<PostRestartResult> {
    let tlog_path = std::path::Path::new(crate::constants::agent_state_dir()).join("tlog.ndjson");
    if tlog_path.exists() {
        if let Some(result) = peek_post_restart_result_from_tlog(role, &tlog_path) {
            return Some(result);
        }
    }
    let path =
        std::path::Path::new(crate::constants::agent_state_dir()).join("post_restart_result.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let saved_role = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
    // Normalise: executor[executor_pool] → executor
    let role_key = post_restart_role_key(role);
    let saved_key = post_restart_role_key(saved_role);
    if role_key != saved_key {
        return None;
    }
    Some(PostRestartResult {
        role: saved_role.to_string(),
        action: v
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or("(unknown)")
            .to_string(),
        result: v
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string(),
        step: v.get("step").and_then(|s| s.as_u64()).unwrap_or(0) as usize,
        tab_id: v.get("tab_id").and_then(|s| s.as_u64()).map(|v| v as u32),
        turn_id: v.get("turn_id").and_then(|s| s.as_u64()),
        endpoint_id: v
            .get("endpoint_id")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        restart_kind: v
            .get("restart_kind")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        signature: v
            .get("signature")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                artifact_signature(&[
                    saved_role,
                    v.get("action").and_then(|a| a.as_str()).unwrap_or("(unknown)"),
                    &v.get("step").and_then(|s| s.as_u64()).unwrap_or(0).to_string(),
                    v.get("endpoint_id").and_then(|s| s.as_str()).unwrap_or(""),
                    &v.get("result")
                        .and_then(|r| r.as_str())
                        .unwrap_or("")
                        .len()
                        .to_string(),
                ])
            }),
    })
}

fn peek_post_restart_result_from_tlog(
    role: &str,
    tlog_path: &std::path::Path,
) -> Option<PostRestartResult> {
    let state = Tlog::replay(tlog_path, SystemState::new(&[], 0)).ok()?;
    let consumed = state
        .post_restart_consumed_signatures
        .get(post_restart_role_key(role))
        .cloned();
    let records = Tlog::read_records(tlog_path).ok()?;
    for record in records.iter().rev() {
        let Event::Effect { event } = &record.event else {
            continue;
        };
        let crate::events::EffectEvent::PostRestartResultRecorded {
            role: saved_role,
            action,
            result,
            step,
            tab_id,
            turn_id,
            endpoint_id,
            restart_kind,
            signature,
        } = event
        else {
            continue;
        };
        if post_restart_role_key(role) != post_restart_role_key(saved_role.as_str()) {
            continue;
        }
        if consumed.as_deref() == Some(signature.as_str()) {
            break;
        }
        return Some(PostRestartResult {
            role: saved_role.clone(),
            action: action.clone(),
            result: result.clone(),
            step: *step,
            tab_id: *tab_id,
            turn_id: *turn_id,
            endpoint_id: endpoint_id.clone(),
            restart_kind: restart_kind.clone(),
            signature: signature.clone(),
        });
    }
    None
}

fn post_restart_role_key(role: &str) -> &str {
    if role.starts_with("executor") {
        "executor"
    } else {
        role
    }
}

/// Read and consume the post-restart result file.  Returns `Some(result)` if the
/// file exists and was written by `role`, then deletes the file.
fn take_post_restart_result(role: &str) -> Option<PostRestartResult> {
    let result = peek_post_restart_result(role)?;
    let tlog_path = std::path::Path::new(crate::constants::agent_state_dir()).join("tlog.ndjson");
    if tlog_path.exists() {
        if let Ok(state) = Tlog::replay(&tlog_path, SystemState::new(&[], 0)) {
            if let Ok(mut writer) = CanonicalWriter::try_new(
                state,
                Tlog::open(&tlog_path),
                std::path::PathBuf::from(crate::constants::workspace()),
            ) {
                let role_key = if role.starts_with("executor") {
                    "executor".to_string()
                } else {
                    role.to_string()
                };
                writer.apply(ControlEvent::PostRestartResultConsumed {
                    role: role_key,
                    signature: result.signature.clone(),
                });
            }
        }
    }
    let path =
        std::path::Path::new(crate::constants::agent_state_dir()).join("post_restart_result.json");
    // Consume — delete so it isn't re-injected on a second restart
    let _ = std::fs::remove_file(&path);
    Some(result)
}

fn restart_resume_banner(role: &str, resume: &PostRestartResult) -> String {
    let agent_type = role_key(role).to_uppercase();
    let tab_id = resume
        .tab_id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "pending".to_string());
    let turn_id = resume
        .turn_id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "pending".to_string());
    let prefix = format!(
        "TAB_ID: {tab_id}\nTURN_ID: {turn_id}\nAGENT_TYPE: {agent_type}\n\nSYSTEM RESTART RESUME\n\
         Resume role: {}\nRestart kind: {}\nEndpoint: {}\nLast completed action: `{}` (step {})\n\
         Continue from the last completed action result below. Do not resend the bootstrap prompt.\n",
        resume.role,
        resume.restart_kind,
        if resume.endpoint_id.is_empty() {
            "unknown"
        } else {
            &resume.endpoint_id
        },
        resume.action,
        resume.step,
    );
    let suffix = "\n\nReturn to the same conversation and continue from this result.";
    render_action_result_sections(&prefix, &resume.result, suffix)
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &[llm_runtime::config::LlmEndpoint], &str
/// Outputs: std::result::Result<&llm_runtime::config::LlmEndpoint, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn find_endpoint<'a>(endpoints: &'a [LlmEndpoint], role: &str) -> Result<&'a LlmEndpoint> {
    endpoints
        .iter()
        .find(|e| e.role.as_deref() == Some(role))
        .ok_or_else(|| anyhow!("no endpoint with role '{role}' in constants"))
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: ()
/// Outputs: std::vec::Vec<llm_runtime::config::LlmEndpoint>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_endpoints() -> Vec<LlmEndpoint> {
    ENDPOINT_SPECS
        .iter()
        .map(|spec| LlmEndpoint {
            id: spec.id.to_string(),
            url: spec.urls.iter().map(|s| s.to_string()).collect(),
            role_markdown: spec.role_markdown.to_string(),
            role: Some(spec.role.to_string()),
            stateful: spec.stateful,
            max_tabs: spec.max_tabs,
        })
        .collect()
}

#[derive(Clone)]
struct PendingSubmitState {
    job: PendingExecutorSubmit,
    started_ms: u64,
    command_id: String,
    endpoint_id: String,
    tabs: TabManagerHandle,
}

#[derive(Clone)]
struct DeferredExecutorCompletion {
    submitted: SubmittedExecutorTurn,
    turn_id: u64,
    tab_id: u32,
    exec_result: String,
}

/// Non-serializable runtime-only state.  Everything serializable now lives in
/// `SystemState` (owned by `CanonicalWriter`); this struct holds the objects
/// that contain live OS handles and are therefore not checkpoint-able.
struct RuntimeState {
    submitted_turns: HashMap<(u32, u64), SubmittedExecutorTurn>,
    executor_submit_inflight: HashMap<usize, PendingSubmitState>,
    timed_out_executor_submits: HashMap<usize, PendingSubmitState>,
    deferred_completions: HashMap<usize, VecDeque<DeferredExecutorCompletion>>,
}

fn new_runtime_state(lanes: &[LaneConfig]) -> RuntimeState {
    let mut deferred_completions = HashMap::new();
    for lane in lanes {
        deferred_completions.insert(lane.index, VecDeque::new());
    }
    RuntimeState {
        submitted_turns: HashMap::new(),
        executor_submit_inflight: HashMap::new(),
        timed_out_executor_submits: HashMap::new(),
        deferred_completions,
    }
}

#[derive(Clone)]
struct SubmittedExecutorTurn {
    tab_id: u32,
    lane: usize,
    lane_label: String,
    command_id: String,
    started_ms: u64,
    actor: String,
    endpoint_id: String,
    tabs: TabManagerHandle,
    steps_used: usize,
}

type ContinuationJoinOutput = (
    SubmittedExecutorTurn,
    u64,
    Result<AgentCompletion>,
    UnboundedReceiver<EffectEvent>,
);

#[derive(Clone)]
struct PendingExecutorSubmit {
    executor_name: String,
    executor_display: String,
    lane_index: usize,
    label: String,
    latest_verify_result: String,
    executor_role: String,
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::option::Option<(u32, u64, std::option::Option<std::string::String>)>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_submit_ack(raw: &str) -> Option<(u32, u64, Option<String>)> {
    let v: Value = serde_json::from_str(raw).ok()?;
    if v.get("submit_ack").and_then(|x| x.as_bool()) != Some(true) {
        return None;
    }
    let tab_id = v.get("tab_id").and_then(|x| x.as_u64())? as u32;
    let turn_id = v.get("turn_id").and_then(|x| x.as_u64())?;
    let command_id = v
        .get("command_id")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    Some((tab_id, turn_id, command_id))
}

fn parsed_completion_command_summary(action: &serde_json::Value) -> String {
    let kind = action
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    match kind {
        "run_command" => jstr(action, "cmd").to_string(),
        "python" => "python".to_string(),
        "read_file" => {
            let path = jstr(action, "path");
            match action.get("line").and_then(|v| v.as_u64()) {
                Some(n) => format!("read_file {}:{}", path, n),
                None => format!("read_file {}", path),
            }
        }
        "list_dir" => format!("list_dir {}", jstr(action, "path")),
        "apply_patch" => "apply_patch".to_string(),
        "message" => {
            let status = jstr(action, "status");
            let summary = action
                .get("payload")
                .and_then(|v| v.get("summary"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("message {} {}", status, summary)
        }
        _ => kind.to_string(),
    }
}

/// Intent: event_append
/// Resource: error
/// Inputs: &app::SubmittedExecutorTurn, usize, u64, u32, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn append_executor_completion_log(
    submitted: &SubmittedExecutorTurn,
    step: usize,
    turn_id: u64,
    tab_id: u32,
    text: &str,
) -> Result<()> {
    let parsed_completion = parsed_executor_completion(text);
    let record = compact_log_record(
        "llm",
        "completion",
        Some(&submitted.actor),
        Some(submitted.lane_label.as_str()),
        Some(&submitted.endpoint_id),
        Some(step),
        Some(turn_id),
        Some(&submitted.command_id),
        parsed_completion.action_summary,
        None,
        parsed_completion.observation,
        parsed_completion.rationale,
        Some(text.to_string()),
        Some(json!({ "tab_id": tab_id })),
    );
    append_action_log_record(&record)
}

struct ParsedExecutorCompletion {
    observation: Option<String>,
    rationale: Option<String>,
    action_summary: Option<Value>,
}

fn parsed_executor_completion(text: &str) -> ParsedExecutorCompletion {
    let parsed = parse_actions(text)
        .ok()
        .and_then(|actions| actions.into_iter().next());
    let observation = parsed
        .as_ref()
        .and_then(|action| action_observation(action))
        .map(str::to_string);
    let rationale = parsed
        .as_ref()
        .and_then(|action| action_rationale(action))
        .map(str::to_string);
    let parsed_action = parsed
        .as_ref()
        .and_then(|action| action.get("action").and_then(|v| v.as_str()))
        .map(str::to_string);
    let parsed_command = parsed
        .as_ref()
        .map(parsed_completion_command_summary)
        .filter(|s| !s.is_empty());
    let action_summary = parsed_action.map(|name| {
            let summary = parsed_command.clone().unwrap_or_else(|| name.clone());
            json!({
                "name": name,
                "summary": summary,
            })
        });
    ParsedExecutorCompletion {
        observation,
        rationale,
        action_summary,
    }
}
