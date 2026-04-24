fn guardrail_reaction_only_action(role: &str) -> Value {
    let _ = role;
    let path = "canon-utils";
    json!({
        "action": "list_dir",
        "observation": "Received reaction-only response; forcing a concrete discovery action.",
        "rationale": "Reaction-only responses are invalid; gather fresh evidence instead.",
        "path": path
    })
}

fn guardrail_diff_message_action(raw: &str, role: &str) -> Value {
    let (from, to, msg_type, status) = default_message_route(role);
    json!({
        "action": "message",
        "from": from,
        "to": to,
        "type": msg_type,
        "status": status,
        "observation": "Model responded with diff-only text; wrapping as message payload.",
        "rationale": "Diff output must be wrapped in a valid message action.",
        "payload": guardrail_diff_message_payload(raw, from, to, msg_type, status)
    })
}

fn guardrail_diff_message_payload(
    raw: &str,
    from: &str,
    to: &str,
    msg_type: &str,
    status: &str,
) -> Value {
    json!({
        "summary": "diff-only output captured",
        "diff_excerpt": truncate(raw, 1500),
        "expected_format": expected_message_format(from, to, msg_type, status)
    })
}

fn apply_error_result(
    role: &str,
    task_context: &str,
    error_streak: &mut usize,
    last_error: &mut Option<String>,
    last_result: &mut Option<String>,
    err_text: &str,
    default_result: String,
) {
    *error_streak = error_streak.saturating_add(1);
    *last_error = Some(err_text.to_string());
    *last_result = Some(default_result);
    if should_force_blocker(*error_streak) {
        *last_result = Some(blocker_escalation_prompt(
            role,
            last_error.as_deref().unwrap_or(err_text),
            task_context,
        ));
    }
}

struct InvalidActionFeedback {
    err_text: String,
    feedback: String,
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &llm_runtime::config::LlmEndpoint, &str, usize, &str, &str, bool, bool, std::option::Option<(&str, u32
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_action_from_raw(
    role: &str,
    endpoint: &LlmEndpoint,
    prompt_kind: &str,
    step: usize,
    exchange_id: &str,
    raw: &str,
    allow_guardrail: bool,
    allow_auto_fill_message: bool,
    trace_context: Option<(&str, u32)>,
) -> Result<Value, InvalidActionFeedback> {
    let log = |event: &str, data: Value| {
        log_message_event(role, endpoint, prompt_kind, step, exchange_id, event, data);
    };
    let trace = |status: &str| {
        if let Some((lane_label, tab_id)) = trace_context {
            append_orchestration_trace(
                "executor_tool_result_forwarded",
                json!({ "lane_name": lane_label, "tab_id": tab_id, "turn_id": exchange_id, "status": status }),
            );
        }
    };

    let actions = match parse_actions(raw) {
        Ok(actions) => actions,
        Err(e) => {
            return handle_parse_actions_error(
                role,
                step,
                raw,
                allow_guardrail,
                &log,
                &trace,
                &e.to_string(),
            );
        }
    };

    let mut action = extract_single_action(role, step, raw, actions, &log, &trace)?;
    let raw_action = action.clone();

    normalize_action_or_feedback(role, raw, &raw_action, &mut action, &log)?;

    if allow_auto_fill_message {
        auto_fill_message_fields(&mut action, role);
    }

    // Always run the base-schema autofill so missing provenance fields (rationale,
    // predicted_next_actions, intent, task_id, objective_id) are populated before
    // validation.  This breaks the schema-rejection→identical-retry loop without
    // suppressing real structural errors.
    ensure_action_base_schema(&mut action);

    validate_action_or_feedback(role, raw, &action, &log)?;

    Ok(action)
}

fn handle_parse_actions_error(
    role: &str,
    step: usize,
    raw: &str,
    allow_guardrail: bool,
    log: &impl Fn(&str, Value),
    trace: &impl Fn(&str),
    err_text: &str,
) -> Result<Value, InvalidActionFeedback> {
    if let Some(guard_action) =
        maybe_guardrail_parse_action(role, raw, allow_guardrail, log, err_text)
    {
        return Ok(guard_action);
    }

    eprintln!(
        "[{role}] step={} parse_error: {}\n[{role}] step={} parse_error_raw: {}",
        step,
        err_text,
        step,
        truncate(raw, MAX_SNIPPET)
    );
    log(
        "llm_parse_error",
        json!({ "error": err_text, "raw": truncate(raw, MAX_SNIPPET) }),
    );
    trace("parse_error");
    Err(InvalidActionFeedback {
        err_text: err_text.to_string(),
        feedback: build_invalid_action_feedback(None, err_text, role),
    })
}

fn maybe_guardrail_parse_action(
    role: &str,
    raw: &str,
    allow_guardrail: bool,
    log: &impl Fn(&str, Value),
    err_text: &str,
) -> Option<Value> {
    if !allow_guardrail {
        return None;
    }
    let guard_action = guardrail_action_from_raw(raw, role)?;
    log(
        "llm_guardrail_action",
        json!({
            "error": err_text,
            "raw": truncate(raw, MAX_SNIPPET),
            "action": guard_action,
        }),
    );
    Some(guard_action)
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, usize, &str, std::vec::Vec<serde_json::Value>, &impl Fn(&str, Value
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_single_action(
    role: &str,
    step: usize,
    raw: &str,
    actions: Vec<Value>,
    log: &impl Fn(&str, Value),
    trace: &impl Fn(&str),
) -> Result<Value, InvalidActionFeedback> {
    if actions.len() != 1 {
        return Err(single_action_count_feedback(role, step, raw, actions.len(), log, trace));
    }

    Ok(actions.into_iter().next().expect("validated single action"))
}

fn single_action_count_feedback(
    role: &str,
    step: usize,
    raw: &str,
    action_count: usize,
    log: &impl Fn(&str, Value),
    trace: &impl Fn(&str),
) -> InvalidActionFeedback {
    let msg = format!("Got {action_count} actions — emit exactly one action per turn.");
    eprintln!("[{role}] step={} {msg}", step);
    log(
        "llm_invalid_action_count",
        json!({ "action_count": action_count, "raw": truncate(raw, MAX_SNIPPET) }),
    );
    trace("invalid_action_count");
    InvalidActionFeedback {
        err_text: msg.clone(),
        feedback: build_invalid_action_feedback(None, &msg, role),
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &serde_json::Value, &mut serde_json::Value, &impl Fn(&str, Value
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn normalize_action_or_feedback(
    role: &str,
    raw: &str,
    raw_action: &Value,
    action: &mut Value,
    log: &impl Fn(&str, Value),
) -> Result<(), InvalidActionFeedback> {
    if let Err(e) = normalize_action(action) {
        let err_text = e.to_string();
        log(
            "llm_invalid_action",
            json!({
                "stage": "normalize_action",
                "error": err_text,
                "raw": truncate(raw, MAX_SNIPPET),
            }),
        );
        return Err(InvalidActionFeedback {
            err_text: err_text.clone(),
            feedback: format!(
                "{}\nFor any mutating retry (`apply_patch`, `plan`, `objectives`, `issue`, or `rename_symbol`), include a non-empty `question` field stating the decision-boundary premise. Return exactly one action.",
                build_invalid_action_feedback(Some(raw_action), &err_text, role)
            ),
        });
    }

    Ok(())
}

/// Intent: validation_gate
/// Resource: error
/// Inputs: &str, &str, &serde_json::Value, &impl Fn(&str, Value
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn validate_action_or_feedback(
    role: &str,
    raw: &str,
    action: &Value,
    log: &impl Fn(&str, Value),
) -> Result<(), InvalidActionFeedback> {
    if let Err(e) = validate_action(action) {
        return Err(handle_invalid_action_error(
            role,
            raw,
            action,
            log,
            &e.to_string(),
        ));
    }

    Ok(())
}

fn handle_invalid_action_error(
    role: &str,
    raw: &str,
    action: &Value,
    log: &impl Fn(&str, Value),
    err_text: &str,
) -> InvalidActionFeedback {
    log_invalid_action_validation(raw, action, log, err_text);
    if let Some(prompt) = corrective_invalid_action_prompt(action, err_text, role) {
        return invalid_action_feedback_with_prompt(action, err_text, role, &prompt);
    }
    if err_text.contains("cargo_test missing 'crate'") {
        return cargo_test_missing_crate_feedback(err_text);
    }
    invalid_action_feedback(action, err_text, role)
}

fn log_invalid_action_validation(
    raw: &str,
    action: &Value,
    log: &impl Fn(&str, Value),
    err_text: &str,
) {
    log(
        "llm_invalid_action",
        json!({
            "stage": "validate_action",
            "error": err_text,
            "raw": truncate(raw, MAX_SNIPPET),
            "action": action.clone(),
        }),
    );
}

fn invalid_action_feedback(action: &Value, err_text: &str, role: &str) -> InvalidActionFeedback {
    InvalidActionFeedback {
        err_text: err_text.to_string(),
        feedback: build_invalid_action_feedback(Some(action), err_text, role),
    }
}

fn invalid_action_feedback_with_prompt(
    action: &Value,
    err_text: &str,
    role: &str,
    prompt: &str,
) -> InvalidActionFeedback {
    InvalidActionFeedback {
        err_text: err_text.to_string(),
        feedback: format!(
            "{}\n\n{}",
            build_invalid_action_feedback(Some(action), err_text, role),
            prompt
        ),
    }
}

fn cargo_test_missing_crate_feedback(err_text: &str) -> InvalidActionFeedback {
    InvalidActionFeedback {
        err_text: err_text.to_string(),
        feedback: format!(
            "Invalid action: {err_text}\nCorrective action required: `cargo_test` must include a `crate` field.\nUse this exact format and fill in the crate name:\n```json\n{{\n  \"action\": \"cargo_test\",\n  \"crate\": \"canon-mini-agent\",\n  \"task_id\": \"<plan task id>\",\n  \"objective_id\": \"<objective id>\",\n  \"intent\": \"Run verification for the current task after the latest change.\",\n  \"observation\": \"Running canon-mini-agent test suite after latest changes.\",\n  \"rationale\": \"Validate that canon-mini-agent tests pass after the latest change.\",\n  \"predicted_next_actions\": [\n    {{\"action\": \"read_file\", \"intent\": \"Inspect the failing source or artifact if the test still fails.\"}},\n    {{\"action\": \"apply_patch\", \"intent\": \"Patch the verified defect if the test output identifies a code issue.\"}}\n  ]\n}}\n```\nFor any mutating retry (`apply_patch`, `plan`, `objectives`, `issue`, or `rename_symbol`), include a non-empty `question` field stating the decision-boundary premise.\nReturn exactly one action."
        ),
    }
}

#[derive(Clone, Debug, Default)]
struct ActionProvenance {
    task_id: Option<String>,
    objective_id: Option<String>,
    intent: Option<String>,
}

impl ActionProvenance {
    fn from_action(action: &Value) -> Self {
        Self {
            task_id: action_task_id(action).map(str::to_string),
            objective_id: action_objective_id(action).map(str::to_string),
            intent: action_intent(action).map(str::to_string),
        }
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, bool, usize, &str, &str, std::option::Option<&str>, std::option::Option<u32>, std::option::Option<u64>, std::option::Option<&str>, &app::ActionProvenance, usize, std::option::Option<&str>
/// Outputs: (std::string::String, std::string::String)
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_agent_prompt(
    role: &str,
    send_system_prompt: bool,
    step: usize,
    initial_prompt: &str,
    system_instructions: &str,
    last_result: Option<&str>,
    last_tab_id: Option<u32>,
    last_turn_id: Option<u64>,
    last_action: Option<&str>,
    last_provenance: &ActionProvenance,
    total_steps: usize,
    last_predicted_next_actions: Option<&str>,
) -> (String, String) {
    let agent_type = role_key(role).to_uppercase();
    let header = format!("TAB_ID: pending\nTURN_ID: pending\nAGENT_TYPE: {agent_type}\n\n");
    if step == 0 {
        (
            if send_system_prompt {
                system_instructions.to_string()
            } else {
                String::new()
            },
            format!("{header}{initial_prompt}"),
        )
    } else {
        let result = last_result.unwrap_or("").to_string();
        let role_schema = if send_system_prompt {
            system_instructions.to_string()
        } else {
            String::new()
        };
        (
            role_schema,
            action_result_prompt(
                last_tab_id,
                last_turn_id,
                agent_type.as_str(),
                &result,
                last_action,
                last_provenance.task_id.as_deref(),
                last_provenance.objective_id.as_deref(),
                last_provenance.intent.as_deref(),
                if role.starts_with("executor") {
                    Some(total_steps)
                } else {
                    None
                },
                last_predicted_next_actions,
            ),
        )
    }
}

fn should_send_system_prompt(
    send_system_prompt: bool,
    endpoint_stateful: bool,
    step: usize,
) -> bool {
    send_system_prompt && (!endpoint_stateful || step == 0)
}

fn enforce_executor_step_limit(
    role: &str,
    total_steps: usize,
    error_streak: &mut usize,
    last_result: &mut Option<String>,
    workspace: &std::path::Path,
) -> bool {
    if role.starts_with("planner")
        && executor_step_limit_exceeded(total_steps, crate::constants::PLANNER_STEP_LIMIT)
    {
        *error_streak = error_streak.saturating_add(1);
        *last_result = Some(planner_step_limit_feedback());
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
        return true;
    }
    if role.starts_with("executor")
        && executor_step_limit_exceeded(total_steps, EXECUTOR_STEP_LIMIT)
    {
        *error_streak = error_streak.saturating_add(1);
        *last_result = Some(executor_step_limit_feedback());
        crate::blockers::record_action_failure_with_writer(
            workspace,
            None,
            role,
            "step_limit",
            &format!("executor reached step limit ({EXECUTOR_STEP_LIMIT})"),
            None,
        );
        return true;
    }
    false
}

fn executor_step_limit_feedback() -> String {
    format!(
        "Step limit reached after {EXECUTOR_STEP_LIMIT} actions.\nPreferred action now: emit a `plan` status update, not a routine handoff message.\n\nPrimary path (use this unless truly blocked):\n```json\n{{\n  \"action\": \"plan\",\n  \"op\": \"set_task_status\",\n  \"task_id\": \"<active_task_id>\",\n  \"status\": \"done\" | \"in_progress\",\n  \"rationale\": \"Evidence-based completion/progress summary.\"\n}}\n```\n\nOnly if blocked/unresolvable, emit one `message` blocker:\n```json\n{{\n  \"action\": \"message\",\n  \"from\": \"executor\",\n  \"to\": \"planner\",\n  \"type\": \"blocker\",\n  \"status\": \"blocked\",\n  \"observation\": \"Progress is blocked by a concrete failure.\",\n  \"rationale\": \"Planner must resolve the blocker before more executor actions.\",\n  \"payload\": {{\n    \"summary\": \"Executor is blocked.\",\n    \"blocker\": \"Root cause\",\n    \"evidence\": \"Exact error text or failed command\",\n    \"required_action\": \"What planner should do next\"\n  }}\n}}\n```"
    )
}

fn planner_step_limit_feedback() -> String {
    use crate::constants::PLANNER_STEP_LIMIT;
    format!(
        "Planning cycle step limit reached ({PLANNER_STEP_LIMIT} actions).\n\
         You must terminate this cycle now with exactly one `message` action.\n\n\
         If the plan already has ready tasks, emit the executor handoff:\n\
         ```json\n\
         {{\n\
           \"action\": \"message\",\n\
           \"from\": \"planner\",\n\
           \"to\": \"executor\",\n\
           \"type\": \"handoff\",\n\
           \"status\": \"ready\",\n\
           \"observation\": \"Plan has ready tasks for execution.\",\n\
           \"rationale\": \"Planner cycle complete; executor takes the ready work.\",\n\
           \"predicted_next_actions\": []\n\
         }}\n\
         ```\n\n\
         Only if genuinely blocked (no ready tasks, external dependency missing), emit a blocker instead:\n\
         ```json\n\
         {{\n\
           \"action\": \"message\",\n\
           \"from\": \"planner\",\n\
           \"to\": \"executor\",\n\
           \"type\": \"blocker\",\n\
           \"status\": \"blocked\",\n\
           \"observation\": \"Describe the blocking condition.\",\n\
           \"rationale\": \"Explain what must change before execution can proceed.\",\n\
           \"payload\": {{\n\
             \"summary\": \"Planner is blocked.\",\n\
             \"blocker\": \"Root cause\",\n\
             \"required_action\": \"What must be resolved externally\"\n\
           }},\n\
           \"predicted_next_actions\": []\n\
         }}\n\
         ```"
    )
}

fn canonical_tlog_read_path(agent_state_dir: &std::path::Path) -> PathBuf {
    let workspace_tlog = PathBuf::from(crate::constants::workspace())
        .join("agent_state")
        .join("tlog.ndjson");
    let agent_state_tlog = agent_state_dir.join("tlog.ndjson");

    let has_data = |path: &Path| {
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.len() > 0)
            .unwrap_or(false)
    };

    if has_data(&workspace_tlog) {
        workspace_tlog
    } else if has_data(&agent_state_tlog) || agent_state_tlog.exists() {
        agent_state_tlog
    } else {
        workspace_tlog
    }
}

#[derive(Clone, Copy)]
enum RecordedMessageKind {
    Inbound,
    ExternalUser,
}

fn normalized_role_key(role: &str) -> String {
    role
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
}

fn recorded_message_projection_path(
    agent_state_dir: &std::path::Path,
    role_key: &str,
    kind: RecordedMessageKind,
) -> PathBuf {
    match kind {
        RecordedMessageKind::Inbound => {
            agent_state_dir.join(format!("last_message_to_{role_key}.json"))
        }
        RecordedMessageKind::ExternalUser => {
            agent_state_dir.join(format!("external_user_message_to_{role_key}.json"))
        }
    }
}

fn recorded_message_consumed_event(
    kind: RecordedMessageKind,
    role: String,
    signature: String,
) -> ControlEvent {
    match kind {
        RecordedMessageKind::Inbound => ControlEvent::InboundMessageConsumed { role, signature },
        RecordedMessageKind::ExternalUser => {
            ControlEvent::ExternalUserMessageConsumed { role, signature }
        }
    }
}

fn pending_recorded_message_from_state(
    state: &SystemState,
    role_key: &str,
    kind: RecordedMessageKind,
) -> Option<(String, String)> {
    match kind {
        RecordedMessageKind::Inbound => state.inbound_messages_pending.get(role_key).map(|msg| {
            let signature = artifact_write_signature(&[
                "inbound_message_consumed",
                role_key,
                &msg.len().to_string(),
            ]);
            (signature, msg.clone())
        }),
        RecordedMessageKind::ExternalUser => None,
    }
}

fn latest_recorded_message_from_tlog(
    agent_state_dir: &std::path::Path,
    role: &str,
    kind: RecordedMessageKind,
) -> Option<(String, String)> {
    let tlog_path = canonical_tlog_read_path(agent_state_dir);
    Tlog::latest_record_by_seq(&tlog_path, |event| {
        let matched = match (kind, event) {
            (
                RecordedMessageKind::Inbound,
                Event::Effect {
                    event:
                        EffectEvent::InboundMessageRecorded {
                            to_role,
                            message,
                            signature,
                            ..
                        },
                },
            ) => Some((to_role, signature, message)),
            (
                RecordedMessageKind::ExternalUser,
                Event::Effect {
                    event:
                        EffectEvent::ExternalUserMessageRecorded {
                            to_role,
                            message,
                            signature,
                        },
                },
            ) => Some((to_role, signature, message)),
            _ => None,
        };
        let (to_role, signature, message) = matched?;
        (to_role == role).then_some((signature, message))
    })
    .ok()?
}

fn canonical_recorded_message_from_tlog(
    agent_state_dir: &std::path::Path,
    state: &SystemState,
    role: &str,
    kind: RecordedMessageKind,
) -> Option<(String, String)> {
    let (signature, message) = latest_recorded_message_from_tlog(agent_state_dir, role, kind)?;
    let consumed_latest = match kind {
        RecordedMessageKind::Inbound => state.inbound_message_signatures.get(role),
        RecordedMessageKind::ExternalUser => state.external_user_message_signatures.get(role),
    }
    .map(String::as_str)
        == Some(signature.as_str());
    if consumed_latest {
        None
    } else {
        Some((signature, message))
    }
}

fn take_recorded_message(
    writer: &mut CanonicalWriter,
    role: &str,
    kind: RecordedMessageKind,
) -> Option<String> {
    let role_key = normalized_role_key(role);
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let path = recorded_message_projection_path(agent_state_dir, &role_key, kind);
    let canonical = pending_recorded_message_from_state(writer.state(), &role_key, kind)
        .or_else(|| canonical_recorded_message_from_tlog(agent_state_dir, writer.state(), &role_key, kind));

    if let Some((signature, message)) = canonical {
        let trimmed = message.trim().to_string();
        writer.apply(recorded_message_consumed_event(kind, role_key, signature));
        let _ = std::fs::remove_file(&path);
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed);
    }
    None
}

fn take_inbound_message(writer: &mut CanonicalWriter, role: &str) -> Option<String> {
    take_recorded_message(writer, role, RecordedMessageKind::Inbound)
}

fn take_inbound_message_without_writer(role: &str) -> Option<String> {
    take_recorded_message_without_writer(role, RecordedMessageKind::Inbound)
}

fn take_recorded_message_without_writer(
    role: &str,
    kind: RecordedMessageKind,
) -> Option<String> {
    let role_key = normalized_role_key(role);
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let tlog_path = canonical_tlog_read_path(agent_state_dir);
    let state = Tlog::replay(&tlog_path, SystemState::new(&[], 0)).ok();
    let canonical = state
        .as_ref()
        .and_then(|state| {
            pending_recorded_message_from_state(state, &role_key, kind)
                .or_else(|| canonical_recorded_message_from_tlog(agent_state_dir, state, &role_key, kind))
        })
        .or_else(|| latest_recorded_message_from_tlog(agent_state_dir, &role_key, kind));

    if let Some((signature, message)) = canonical {
        if let Some(state) = state {
            if let Ok(mut writer) = CanonicalWriter::try_new(
                state,
                Tlog::open(&tlog_path),
                PathBuf::from(crate::constants::workspace()),
            ) {
                let _ = writer.try_apply(recorded_message_consumed_event(
                    kind,
                    role_key.clone(),
                    signature,
                ));
            }
        }
        let path = recorded_message_projection_path(agent_state_dir, &role_key, kind);
        let _ = std::fs::remove_file(&path);
        let trimmed = message.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed);
    }
    None
}

fn take_external_user_message(writer: &mut CanonicalWriter, role: &str) -> Option<String> {
    take_recorded_message(writer, role, RecordedMessageKind::ExternalUser)
}

fn take_external_user_message_without_writer(role: &str) -> Option<String> {
    take_recorded_message_without_writer(role, RecordedMessageKind::ExternalUser)
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
fn append_external_user_message_to_prompt(prompt: &mut String, inbound: &str) {
    let parsed = serde_json::from_str::<Value>(inbound).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| value.get("message"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(inbound.trim());

    prompt.push_str("\n\nExternal user request:\n");
    prompt.push_str(message);
    prompt.push('\n');
    prompt.push_str(
        "\nRespond under canonical law and current system policy. If you choose a direct result reply message this cycle, address it to `user` using an allowed message type.\n",
    );
}

fn executor_result_highlight_lines(executor_result: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in executor_result.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        let keep = lower.contains("action=")
            || lower.contains("apply_patch")
            || lower.contains("run_command")
            || lower.contains("cargo check")
            || lower.contains("cargo test")
            || lower.contains(" status:")
            || lower.contains(" ok")
            || lower.contains(" failed")
            || lower.contains("error:");
        if !keep {
            continue;
        }
        let normalized = truncate(trimmed, 180).to_string();
        if out.iter().any(|existing| existing == &normalized) {
            continue;
        }
        out.push(normalized);
        if out.len() >= 10 {
            break;
        }
    }
    out
}

fn append_executor_result_summary(out: &mut String, executor_result: &str) {
    let highlights = executor_result_highlight_lines(executor_result);
    if highlights.is_empty() {
        out.push_str(&format!(
            "executor_result: {}\n",
            truncate(executor_result.trim(), 280)
        ));
        return;
    }
    out.push_str("executor_result_highlights:\n");
    for line in highlights {
        out.push_str(&format!("- {line}\n"));
    }
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
fn summarize_inbound_message(inbound: &str, role: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(inbound) else {
        return truncate(inbound.trim(), 1600).to_string();
    };
    let mut out = String::new();
    let from = value.get("from").and_then(Value::as_str).unwrap_or("?");
    let to = value.get("to").and_then(Value::as_str).unwrap_or("?");
    let ty = value.get("type").and_then(Value::as_str).unwrap_or("?");
    let status = value.get("status").and_then(Value::as_str).unwrap_or("?");
    out.push_str(&format!("from={from} to={to} type={ty} status={status}\n"));

    if let Some(intent) = value.get("intent").and_then(Value::as_str) {
        let intent = intent.trim();
        if !intent.is_empty() {
            out.push_str(&format!("intent: {}\n", truncate(intent, 240)));
        }
    }
    if let Some(observation) = value.get("observation").and_then(Value::as_str) {
        let observation = observation.trim();
        if !observation.is_empty() {
            out.push_str(&format!("observation: {}\n", truncate(observation, 280)));
        }
    }
    if let Some(payload) = value.get("payload").and_then(Value::as_object) {
        for key in [
            "summary",
            "blocker",
            "evidence",
            "required_action",
            "expected_format",
        ] {
            if let Some(text) = payload.get(key).and_then(Value::as_str) {
                let text = text.trim();
                if !text.is_empty() {
                    out.push_str(&format!("{key}: {}\n", truncate(text, 280)));
                }
            }
        }
        if let Some(executor_result) = payload.get("executor_result").and_then(Value::as_str) {
            let executor_result = executor_result.trim();
            if !executor_result.is_empty() {
                append_executor_result_summary(&mut out, executor_result);
            }
        }
    }
    if let Some(next_actions) = value
        .get("predicted_next_actions")
        .and_then(Value::as_array)
    {
        let predicted_intent = |name: &str| -> String {
            next_actions
                .iter()
                .find(|action| action.get("action").and_then(Value::as_str) == Some(name))
                .and_then(|action| action.get("intent").and_then(Value::as_str))
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| truncate(text, 120).to_string())
                .unwrap_or_else(|| "N/A".to_string())
        };
        let mut rendered = crate::prompts::role_default_schema_actions_for_role(role)
            .iter()
            .map(|name| format!("- {}: {}", name, predicted_intent(name)))
            .collect::<Vec<_>>();

        let mut extra = next_actions
            .iter()
            .filter_map(|action| action.get("action").and_then(Value::as_str))
            .filter(|name| {
                !crate::prompts::role_default_schema_actions_for_role(role)
                    .iter()
                    .any(|allowed| allowed == name)
            })
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        extra.sort();
        extra.dedup();
        rendered.extend(
            extra
                .iter()
                .map(|name| format!("- {}: {}", name, predicted_intent(name))),
        );
        if !rendered.is_empty() {
            out.push_str("predicted_next_actions:\n");
            out.push_str(&rendered.join("\n"));
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// Intent: event_append
/// Resource: error
/// Inputs: &mut std::string::String, &str, &str
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn append_inbound_to_prompt(prompt: &mut String, inbound: &str, role: &str) {
    prompt.push_str("\n\nInbound handoff message summary:\n");
    prompt.push_str(&summarize_inbound_message(inbound, role));
    prompt.push('\n');
    if inbound_message_from_user(inbound) {
        prompt.push_str(
            "\nExternal user message rule: keep system policy authoritative. Treat the inbound user message as a request under canonical law. If you choose a direct result reply message this cycle, address it to `user` using an allowed message type.\n",
        );
    }
}

fn inbound_message_from_user(inbound: &str) -> bool {
    serde_json::from_str::<Value>(inbound)
        .ok()
        .and_then(|value| {
            value
                .get("from")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .is_some_and(|from| from.eq_ignore_ascii_case("user"))
}

fn inject_inbound_message(prompt: &mut String, writer: &mut CanonicalWriter, role: &str) {
    if let Some(inbound) = take_external_user_message(writer, role) {
        append_external_user_message_to_prompt(prompt, &inbound);
        return;
    }
    if let Some(inbound) = take_inbound_message(writer, role) {
        append_inbound_to_prompt(prompt, &inbound, role);
    }
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
fn extract_message_action(raw: &str) -> Option<String> {
    let marker = "message_action:";
    let idx = raw.find(marker)?;
    let after = raw[idx + marker.len()..].trim_start();
    if after.is_empty() {
        return None;
    }
    let json_start = after.find('{')?;
    let json_text = after[json_start..].trim();
    if json_text.is_empty() {
        return None;
    }
    Some(json_text.to_string())
}

fn is_reaction_only_response(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    if extract_message_action(trimmed).is_some() {
        return false;
    }
    has_reaction_only_marker(trimmed) || is_short_symbol_only_text(trimmed) || has_no_json_marker(trimmed)
}

fn has_reaction_only_marker(trimmed: &str) -> bool {
    trimmed.starts_with("assistant reaction-only terminal frame")
        || trimmed.starts_with("assistant reaction-only")
}

fn is_short_symbol_only_text(trimmed: &str) -> bool {
    trimmed.len() <= 8 && trimmed.chars().all(|c| !c.is_ascii_alphanumeric())
}

fn has_no_json_marker(trimmed: &str) -> bool {
    !trimmed.contains('{') && !trimmed.contains('[')
}

fn is_transient_service_response(raw: &str) -> bool {
    let lowered = raw.to_ascii_lowercase();
    lowered.contains("having trouble processing your request")
        || (lowered.contains("i'm sorry")
            && lowered.contains("please try again")
            && lowered.contains("processing"))
}

fn apply_scheduled_phase_if_changed(writer: &mut CanonicalWriter, phase: Option<&str>) -> bool {
    if writer.state().scheduled_phase.as_deref() == phase {
        return false;
    }
    writer.apply(ControlEvent::ScheduledPhaseSet {
        phase: phase.map(str::to_string),
    });
    true
}

fn apply_planner_pending_if_changed(writer: &mut CanonicalWriter, pending: bool) -> bool {
    if writer.state().planner_pending == pending {
        return false;
    }
    writer.apply(ControlEvent::PlannerPendingSet { pending });
    true
}
