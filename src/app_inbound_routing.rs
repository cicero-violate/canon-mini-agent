fn apply_diagnostics_pending_if_changed(writer: &mut CanonicalWriter, pending: bool) -> bool {
    if writer.state().diagnostics_pending == pending {
        return false;
    }
    writer.apply(ControlEvent::DiagnosticsPendingSet { pending });
    true
}

fn apply_lane_pending_if_changed(
    writer: &mut CanonicalWriter,
    lane_id: usize,
    pending: bool,
) -> bool {
    let current = writer
        .state()
        .lanes
        .get(&lane_id)
        .map(|lane| lane.pending)
        .unwrap_or(false);
    if current == pending {
        return false;
    }
    writer.apply(ControlEvent::LanePendingSet { lane_id, pending });
    true
}

fn apply_wake_signals(writer: &mut CanonicalWriter) {
    let state_snapshot = writer.state().clone();
    let (inputs, signature_map) = collect_wake_signal_inputs(&state_snapshot);
    let wake_inputs_debug = inputs
        .iter()
        .map(|input| format!("{}@{}", input.role, input.modified_ms))
        .collect::<Vec<_>>()
        .join(", ");

    let semantic_control = SemanticControlState::new(
        state_snapshot.scheduled_phase.clone(),
        state_snapshot.planner_pending,
        state_snapshot.diagnostics_pending,
        state_snapshot.active_blocker_to_verifier,
    );
    let decision = decide_wake_signals(semantic_control.active_blocker_to_verifier, &inputs);
    let Some(role) = decision.scheduled_phase.as_deref() else {
        return;
    };
    let selected_modified_ms = inputs
        .iter()
        .find(|input| input.role == role)
        .map(|input| input.modified_ms)
        .unwrap_or(0);

    apply_scheduled_phase_if_changed(writer, Some(role));
    let wake_signature = signature_map.get(role).cloned();
    let mut clear_wake_signal = !decision.executor_wake;
    if decision.planner_pending {
        apply_planner_pending_if_changed(writer, true);
    }
    if decision.diagnostics_pending {
        apply_diagnostics_pending_if_changed(writer, true);
    }
    if decision.executor_wake {
        let lane_ids: Vec<usize> = writer.state().lanes.keys().copied().collect();
        for lane_id in lane_ids {
            let (pending, in_progress) = {
                let state = writer.state();
                let lane = state.lanes.get(&lane_id);
                (
                    lane.map(|l| l.pending).unwrap_or(false),
                    lane.and_then(|l| l.in_progress_by.as_ref()).is_some(),
                )
            };
            if pending {
                clear_wake_signal = true;
                continue;
            }
            if in_progress {
                if lane_has_stale_executor_claim(writer.state(), lane_id) {
                    eprintln!(
                        "[orchestrate] wake_signal_recovered_stale_lane: role=executor lane={} reason=stale_in_progress_without_live_work",
                        lane_id
                    );
                    writer.apply(ControlEvent::LaneInProgressSet {
                        lane_id,
                        actor: None,
                    });
                    writer.apply(ControlEvent::LaneNextSubmitAtSet { lane_id, ms: 0 });
                    clear_wake_signal |= apply_lane_pending_if_changed(writer, lane_id, true);
                }
                continue;
            }
            clear_wake_signal |= apply_lane_pending_if_changed(writer, lane_id, true);
            // Do NOT clear in_progress_by here. If the lane already has a submit
            // in flight, clearing ownership causes a double-submit on the next tick
            // (claim_next_lane sees pending=true + in_progress_by=None and spawns a
            // second request while the first is still running). The wake effect
            // is preserved by retaining the wake signal until an idle lane can
            // actually be marked pending.
        }
    }
    let suppress_deferred_repeat_log = !clear_wake_signal
        && role == "executor"
        && should_suppress_repeated_executor_deferred_log(selected_modified_ms);
    if !suppress_deferred_repeat_log {
        eprintln!(
            "[orchestrate] wake_signal_selected: role={} planner_pending={} diagnostics_pending={} executor_wake={} inputs=[{}]",
            role,
            decision.planner_pending,
            decision.diagnostics_pending,
            decision.executor_wake,
            wake_inputs_debug,
        );
    }
    if clear_wake_signal {
        if let Some(signature) = wake_signature {
            writer.apply(ControlEvent::WakeSignalConsumed {
                role: role.to_string(),
                signature,
            });
        }
    }
    if clear_wake_signal {
        clear_repeated_executor_deferred_log_memory(role);
        eprintln!("[orchestrate] wake_signal_triggered: role={role}");
    } else if !suppress_deferred_repeat_log {
        eprintln!(
            "[orchestrate] wake_signal_deferred: role={} reason=all_executor_lanes_busy",
            role
        );
    }
}

fn lane_has_stale_executor_claim(state: &SystemState, lane_id: usize) -> bool {
    let Some(lane) = state.lanes.get(&lane_id) else {
        return false;
    };
    if lane.pending || lane.in_progress_by.is_none() {
        return false;
    }
    if state.lane_submit_active(lane_id) || state.lane_in_flight(lane_id) {
        return false;
    }
    !state
        .submitted_turn_ids
        .values()
        .any(|submitted| submitted.lane_id == lane_id)
}

fn should_suppress_repeated_executor_deferred_log(modified_ms: u64) -> bool {
    let map = repeated_executor_deferred_log_memory();
    let mut guard = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let last = guard.get("executor").copied();
    guard.insert("executor".to_string(), modified_ms);
    last == Some(modified_ms)
}

fn clear_repeated_executor_deferred_log_memory(role: &str) {
    if role != "executor" {
        return;
    }
    let map = repeated_executor_deferred_log_memory();
    let mut guard = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.remove("executor");
}

fn repeated_executor_deferred_log_memory(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    static LAST_DEFERRED_BY_ROLE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, u64>>,
    > = std::sync::OnceLock::new();
    LAST_DEFERRED_BY_ROLE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn collect_wake_signal_inputs(
    state: &SystemState,
) -> (
    Vec<WakeSignalInput>,
    std::collections::HashMap<&'static str, String>,
) {
    let mut inputs = Vec::new();
    let mut signature_map = std::collections::HashMap::new();

    // Primary: read canonical pending signals directly from SystemState.
    // These survive restart via tlog replay — no file scan needed.
    for (role, (ts_ms, signature)) in &state.wake_signals_pending {
        let role_key: &'static str = match role.as_str() {
            "planner" => "planner",
            "executor" => "executor",
            _ => continue,
        };
        if !runtime_role_enabled(role_key) {
            continue;
        }
        inputs.push(WakeSignalInput {
            role: role_key,
            modified_ms: *ts_ms,
        });
        signature_map.insert(role_key, signature.clone());
    }

    (inputs, signature_map)
}

fn record_canonical_inbound_message(
    workspace: &Path,
    from_role: &str,
    to_role: &str,
    message: &str,
) -> Result<String> {
    let signature = artifact_write_signature(&[
        "inbound_message",
        from_role,
        to_role,
        &message.len().to_string(),
        message,
    ]);
    record_effect_for_workspace(
        workspace,
        EffectEvent::InboundMessageRecorded {
            from_role: from_role.to_string(),
            to_role: to_role.to_string(),
            message: message.to_string(),
            signature: signature.clone(),
        },
    )?;
    Ok(signature)
}

/// Intent: pure_transform
/// Provenance: generated
fn normalize_executor_completion_target<'a>(to_role: &'a str) -> &'a str {
    if to_role.eq_ignore_ascii_case("executor") {
        eprintln!(
            "[orchestrate] executor→executor message detected; redirecting to planner \
             to break self-wake stall loop"
        );
        "planner"
    } else if !to_role.eq_ignore_ascii_case("planner") && !to_role.eq_ignore_ascii_case("executor") {
        eprintln!(
            "[orchestrate] two-role mode rerouting executor message target `{}` -> `planner`",
            to_role
        );
        "planner"
    } else {
        to_role
    }
}

/// Intent: canonical_write
/// Provenance: generated
fn persist_non_planner_inbound_message(
    writer: &mut CanonicalWriter,
    from_role: &str,
    to_key: &str,
    action_text: &str,
) {
    let workspace = Path::new(crate::constants::workspace());
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let msg_sig = artifact_write_signature(&[
        "inbound_message",
        from_role,
        to_key,
        &action_text.len().to_string(),
        action_text,
    ]);
    writer.apply(ControlEvent::InboundMessageQueued {
        role: to_key.to_string(),
        content: action_text.to_string(),
        signature: msg_sig,
    });
    let wake_sig = artifact_write_signature(&["wake", to_key, &now_ms().to_string()]);
    writer.apply(ControlEvent::WakeSignalQueued {
        role: to_key.to_string(),
        signature: wake_sig,
        ts_ms: now_ms(),
    });

    if let Err(err) = record_canonical_inbound_message(workspace, from_role, to_key, action_text) {
        writer.record_violation(
            "executor_completion_message",
            &format!("failed to record canonical message for {to_key}: {err:#}"),
        );
    }
    let msg_path = agent_state_dir.join(format!("last_message_to_{to_key}.json"));
    if let Err(err) = persist_agent_state_projection(
        &msg_path,
        action_text,
        &format!("executor_completion_message:{to_key}"),
    ) {
        writer.record_violation(
            "executor_completion_message",
            &format!("failed to persist message for {to_key}: {err:#}"),
        );
    }
}

/// Intent: canonical_write
/// Provenance: generated
fn persist_planner_message(writer: &mut CanonicalWriter, action: &Value) {
    let workspace = Path::new(crate::constants::workspace());
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let action_text = serde_json::to_string_pretty(action).unwrap_or_default();
    let from_role = action
        .get("from")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    let msg_signature = artifact_write_signature(&[
        "inbound_message",
        from_role,
        "planner",
        &action_text.len().to_string(),
        &action_text,
    ]);
    // Canonical event — survives restart via SystemState replay.
    writer.apply(ControlEvent::InboundMessageQueued {
        role: "planner".to_string(),
        content: action_text.clone(),
        signature: msg_signature.clone(),
    });

    let wake_signature = artifact_write_signature(&["wake", "planner", &now_ms().to_string()]);
    writer.apply(ControlEvent::WakeSignalQueued {
        role: "planner".to_string(),
        signature: wake_signature,
        ts_ms: now_ms(),
    });

    // Secondary: physical files kept for external tooling / backward compat.
    let planner_path = agent_state_dir.join("last_message_to_planner.json");
    if let Err(err) =
        record_canonical_inbound_message(workspace, from_role, "planner", &action_text)
    {
        eprintln!("[orchestrate] canonical message record failed: {err:#}");
    }
    if let Err(err) =
        persist_agent_state_projection(&planner_path, &action_text, "planner_handoff_message")
    {
        eprintln!("[orchestrate] physical planner message write failed: {err:#}");
    }
}

/// Intent: canonical_write
/// Effects: writes_artifact, writes_state, transitions_state
/// Provenance: generated
fn persist_planner_blocker_message(writer: &mut CanonicalWriter, action: &Value) -> bool {
    let evidence = action
        .get("payload")
        .and_then(|payload| payload.get("evidence"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let _ = std::fs::create_dir_all(agent_state_dir);
    if !evidence.is_empty() {
        let evidence_hash = artifact_write_signature(&["planner_blocker_evidence", &evidence]);
        if writer.state().planner_blocker_evidence_hash == evidence_hash {
            return false;
        }
        writer.apply(ControlEvent::PlannerBlockerEvidenceSet { evidence_hash });
        let evidence_path = agent_state_dir.join("last_planner_blocker_evidence.txt");
        let _ = std::fs::write(&evidence_path, &evidence);
    }
    persist_planner_message(writer, action);
    true
}

fn invariant_id_from_reason(reason: &str) -> Option<&str> {
    let start = reason.find("[id=")? + 4;
    let end = reason[start..].find(']')? + start;
    let id = reason[start..end].trim();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

/// Intent: route_gate
/// Provenance: generated
fn route_gate_blocker_message(reason: &str) -> Value {
    let summary = match invariant_id_from_reason(reason) {
        Some(id) => format!("Executor dispatch blocked by enforced invariant {id}"),
        None => "Executor dispatch blocked by enforced route invariant".to_string(),
    };
    let blocker = if reason.contains("does not exist") {
        "Plan references a path that does not exist yet"
    } else {
        "Executor dispatch blocked by an enforced invariant"
    };
    let required_action = if reason.contains("does not exist") {
        "Revise the plan so the target is created before it is referenced, or retarget the action to an existing path"
    } else {
        "Revise the plan or workspace state so the blocked invariant no longer fires"
    };
    json!({
        "action": "message",
        "from": "executor",
        "to": "planner",
        "type": "blocker",
        "status": "blocked",
        "observation": "Executor routing is blocked by an enforced invariant; planner must repair the plan before more executor work is dispatched.",
        "rationale": "Returning a structured blocker to the planner is more actionable than repeating route-gate stderr lines.",
        "predicted_next_actions": [
            {
                "action": "read_file",
                "intent": "Inspect the current plan and relevant artifacts to locate the invalid path reference."
            },
            {
                "action": "message",
                "intent": "Report a repaired handoff or a narrower blocker after updating the plan."
            }
        ],
        "payload": build_blocker_payload(
            &summary,
            blocker,
            reason,
            required_action,
            "error",
        ),
    })
}

struct LlmResponseContext<'a> {
    role: &'a str,
    endpoint: &'a LlmEndpoint,
    prompt_kind: &'a str,
    submit_only: bool,
    writer: Option<&'a mut CanonicalWriter>,
    effect_tx: Option<UnboundedSender<EffectEvent>>,
}

fn full_exchange_path(kind: &str, ts_ms: u64, who: &str, step: usize) -> PathBuf {
    PathBuf::from(crate::constants::agent_state_dir())
        .join("llm_full")
        .join(format!("{ts_ms:013}_{kind}_{who}_message_{step:04}.txt"))
}

/// Intent: canonical_write
/// Provenance: generated
fn write_full_exchange(kind: &str, ts_ms: u64, who: &str, step: usize, raw: &str) {
    let path = full_exchange_path(kind, ts_ms, who, step);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, raw);
}

impl<'a> LlmResponseContext<'a> {
    fn record_effect(&mut self, effect: EffectEvent) {
        if let Some(writer) = self.writer.as_deref_mut() {
            writer.record_effect(effect);
            return;
        }
        if let Some(tx) = self.effect_tx.as_ref() {
            let _ = tx.send(effect);
        }
    }

    fn request_log_who(role: &str, has_role_schema: bool) -> &str {
        if has_role_schema {
            "system"
        } else {
            role
        }
    }

    fn request_raw_prompt(prompt: &str, trimmed_role_schema: &str, has_role_schema: bool) -> String {
        if has_role_schema {
            format!("{}\n\n{}", trimmed_role_schema, prompt)
        } else {
            prompt.to_string()
        }
    }

    fn log_request_message_event(
        &self,
        step: usize,
        exchange_id: &str,
        prompt: &str,
        role_schema: &str,
    ) {
        log_message_event(
            self.role,
            self.endpoint,
            self.prompt_kind,
            step,
            exchange_id,
            "llm_request",
            json!({
                "submit_only": self.submit_only,
                "prompt_bytes": prompt.len(),
                "role_schema_bytes": role_schema.len(),
                "prompt": truncate(prompt, MAX_SNIPPET),
            }),
        );
    }

    fn record_request_input_effect(
        &mut self,
        step: usize,
        exchange_id: &str,
        prompt: &str,
        role_schema: &str,
    ) {
        self.record_effect(crate::events::EffectEvent::LlmTurnInput {
            tab_id: None,
            turn_id: None,
            role: self.role.to_string(),
            agent_type: role_key(self.role).to_uppercase(),
            step,
            command_id: exchange_id.to_string(),
            endpoint_id: self.endpoint.id.clone(),
            prompt_hash: crate::logging::stable_hash_hex(prompt),
            prompt_bytes: prompt.len(),
            role_schema_bytes: role_schema.len(),
            submit_only: self.submit_only,
        });
    }

    /// Intent: transport_effect
    /// Provenance: generated
    fn log_request(&mut self, step: usize, exchange_id: &str, prompt: &str, role_schema: &str) {
        let ts_ms = crate::logging::now_ms();
        let trimmed_role_schema = role_schema.trim_end();
        let has_role_schema = !trimmed_role_schema.trim().is_empty();
        let who = Self::request_log_who(self.role, has_role_schema);
        let raw_prompt = Self::request_raw_prompt(prompt, trimmed_role_schema, has_role_schema);
        self.log_request_message_event(step, exchange_id, prompt, role_schema);
        self.record_request_input_effect(step, exchange_id, prompt, role_schema);
        // Downstream debug projection: canonical effect first, flat-file snapshot second.
        write_full_exchange("sent", ts_ms, who, step, &raw_prompt);
        trace_message_forwarded(
            self.role,
            self.prompt_kind,
            step,
            &self.endpoint.id,
            self.submit_only,
            prompt.len(),
        );
    }

    fn log_response(
        &mut self,
        step: usize,
        exchange_id: &str,
        raw: &str,
        tab_id: Option<u32>,
        turn_id: Option<u64>,
    ) {
        let ts_ms = crate::logging::now_ms();
        trace_message_received(
            self.role,
            self.prompt_kind,
            step,
            &self.endpoint.id,
            self.submit_only,
            raw.len(),
        );
        log_message_event(
            self.role,
            self.endpoint,
            self.prompt_kind,
            step,
            exchange_id,
            "llm_response",
            json!({
                "submit_only": self.submit_only,
                "response_bytes": raw.len(),
                "raw": truncate(raw, MAX_SNIPPET),
            }),
        );
        let json_body = raw
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        let action_kind = serde_json::from_str::<serde_json::Value>(json_body)
            .ok()
            .and_then(|v| {
                v.get("action")
                    .and_then(|a| a.as_str())
                    .map(str::to_string)
            });
        self.record_effect(crate::events::EffectEvent::LlmTurnOutput {
            tab_id,
            turn_id,
            role: self.role.to_string(),
            step,
            command_id: exchange_id.to_string(),
            endpoint_id: self.endpoint.id.clone(),
            response_bytes: raw.len(),
            response_hash: crate::logging::stable_hash_hex(raw),
            action_kind,
            raw: raw.to_string(),
        });
        // Downstream debug projection: canonical effect first, flat-file snapshot second.
        write_full_exchange("received", ts_ms, self.role, step, raw);
    }

    fn handle_submit_ack(&self, step: usize, exchange_id: &str, raw: &str) -> Option<String> {
        if !self.submit_only {
            return None;
        }
        if let Ok(mut ack) = serde_json::from_str::<Value>(raw) {
            if ack.get("submit_ack").and_then(|v| v.as_bool()) == Some(true) {
                ack["command_id"] = Value::String(exchange_id.to_string());
                eprintln!("[{}] step={} submit_ack={}", self.role, step, raw);
                log_message_event(
                    self.role,
                    self.endpoint,
                    self.prompt_kind,
                    step,
                    exchange_id,
                    "llm_submit_ack",
                    ack.clone(),
                );
                append_orchestration_trace(
                    "llm_message_processed",
                    json!({
                        "role": self.role,
                        "prompt_kind": self.prompt_kind,
                        "step": step,
                        "endpoint_id": self.endpoint.id,
                        "submit_ack": ack,
                    }),
                );
                return Some(ack.to_string());
            }
        }
        None
    }

    fn log_error(&self, step: usize, exchange_id: &str, error: &str) {
        log_message_event(
            self.role,
            self.endpoint,
            self.prompt_kind,
            step,
            exchange_id,
            "llm_error",
            json!({
                "error": error,
            }),
        );
    }

    fn handle_reaction_only(
        &self,
        step: usize,
        exchange_id: &str,
        raw: &str,
        reaction_only_streak: &mut usize,
        error_streak: &mut usize,
        last_error: &mut Option<String>,
    ) -> bool {
        if !is_reaction_only_response(raw) {
            return false;
        }
        *reaction_only_streak = reaction_only_streak.saturating_add(1);
        log_message_event(
            self.role,
            self.endpoint,
            self.prompt_kind,
            step,
            exchange_id,
            "llm_reaction_only",
            json!({
                "raw": truncate(raw, MAX_SNIPPET),
            }),
        );
        if !should_force_blocker(*reaction_only_streak) {
            *error_streak = error_streak.saturating_add(1);
            *last_error = Some("reaction_only_response".to_string());
            eprintln!(
                "[{}] step={} reaction_only_response retry {}",
                self.role, step, *reaction_only_streak
            );
        }
        true
    }
}

async fn continue_executor_completion(
    submitted: &SubmittedExecutorTurn,
    active_tab_id: u32,
    completion_text: &str,
    turn_id: u64,
    endpoint: &LlmEndpoint,
    bridge: &WsBridge,
    workspace: &Path,
    tabs: &TabManagerHandle,
    effect_tx: Option<UnboundedSender<EffectEvent>>,
) -> Result<AgentCompletion> {
    let role = submitted.actor.as_str();
    let prompt_kind = "executor";
    let step = 1usize;
    let command_id = submitted.command_id.as_str();
    let executor_system = system_instructions(AgentPromptKind::Executor);

    let action = match parse_action_from_raw(
        role,
        endpoint,
        prompt_kind,
        step,
        command_id,
        completion_text,
        true,
        true,
        Some((submitted.lane_label.as_str(), active_tab_id)),
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
            if let Some(tx) = effect_tx.as_ref() {
                let result = invalid.feedback.clone();
                let _ = tx.send(EffectEvent::ActionResultRecorded {
                    role: role.to_string(),
                    step,
                    command_id: command_id.to_string(),
                    action_kind: "invalid_action".to_string(),
                    task_id: None,
                    objective_id: None,
                    ok: false,
                    result_bytes: result.len(),
                    result_hash: crate::logging::stable_hash_hex(&result),
                    result,
                });
            }
            let agent_type = role.to_uppercase();
            let retry_prompt = action_result_prompt(
                Some(active_tab_id),
                Some(turn_id),
                agent_type.as_str(),
                &invalid.feedback,
                Some("invalid_action"),
                None,
                None,
                None,
                Some(submitted.steps_used),
                None,
            );
            return run_agent(
                role,
                prompt_kind,
                &executor_system,
                retry_prompt,
                endpoint,
                bridge,
                workspace,
                tabs,
                None,
                effect_tx.clone(),
                false,
                true,
                true,
                submitted.steps_used,
            )
            .await
            .map_err(|e| anyhow!("executor invalid_action recovery failed: {e}"));
        }
    };

    let (done, out) = process_action_and_execute(
        role,
        prompt_kind,
        endpoint,
        workspace,
        step,
        command_id,
        &action,
        true,
        None,
    )?;
    if done {
        return Ok(
            if action.get("action").and_then(|v| v.as_str()) == Some("message") {
                AgentCompletion::MessageAction {
                    action,
                    summary: out,
                }
            } else {
                AgentCompletion::Summary(out)
            },
        );
    }

    append_orchestration_trace(
        "executor_tool_result_forwarded",
        json!({
            "lane_name": submitted.lane_label,
            "tab_id": active_tab_id,
            "command_id": command_id,
            "action": action.get("action").and_then(|v| v.as_str()),
            "result_bytes": out.len(),
        }),
    );

    let agent_type = role.to_uppercase();
    let provenance = ActionProvenance::from_action(&action);
    run_agent(
        role,
        prompt_kind,
        &executor_system,
        action_result_prompt(
            Some(active_tab_id),
            Some(turn_id),
            agent_type.as_str(),
            &out,
            action.get("action").and_then(|v| v.as_str()),
            provenance.task_id.as_deref(),
            provenance.objective_id.as_deref(),
            provenance.intent.as_deref(),
            Some(submitted.steps_used),
            None,
        ),
        endpoint,
        bridge,
        workspace,
        tabs,
        None,
        effect_tx,
        false,
        true,
        true,
        submitted.steps_used,
    )
    .await
}
