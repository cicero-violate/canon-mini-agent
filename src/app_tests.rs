#[cfg(test)]
mod tests {
    use super::{
        action_retry_fingerprint, canonical_recorded_message_from_tlog, collect_wake_signal_inputs,
        classify_planner_action_result_class, ensure_workspace_artifact_baseline,
        executor_step_limit_feedback,
        has_actionable_objectives, inbound_message_from_user, invariant_id_from_reason,
        is_chromium_transport_error, lane_has_stale_executor_claim,
        local_transport_blocker_message, plan_has_incomplete_tasks, route_gate_blocker_message,
        planner_completion_allows_executor_dispatch, should_reject_solo_self_complete,
        RecordedMessageKind,
        take_external_user_message_without_writer, take_inbound_message_without_writer,
        verifier_confirmed_with_plan_text, ActionProvenance,
    };
    use crate::constants::{ISSUES_FILE, MASTER_PLAN_FILE, VIOLATIONS_FILE};
    use crate::events::EffectEvent;
    use crate::logging::{artifact_write_signature, record_effect_for_workspace};
    use crate::system_state::SystemState;
    use crate::{set_agent_state_dir, set_workspace};
    use serde_json::json;
    use std::fs;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn global_state_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn temp_workspace(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "canon-mini-agent-app-{label}-{}-{}",
            std::process::id(),
            unique
        ))
    }

    #[test]
    fn route_gate_source_injects_inv_171c039a_tuple_before_evaluation() {
        let source = include_str!("app_planner_executor.rs");
        let invalid_route_count = source
            .find("let orchestrator_invalid_route_count = crate::blockers::count_class_recent(")
            .expect("missing orchestrator invalid_route count wiring");
        let actor_kind_insert = source[invalid_route_count..]
            .find("state.insert(\"actor_kind\".to_string(), \"orchestrator\".to_string());")
            .map(|offset| invalid_route_count + offset)
            .expect("missing orchestrator actor_kind injection");
        let error_class_insert = source[actor_kind_insert..]
            .find("state.insert(\"error_class\".to_string(), \"invalid_route\".to_string());")
            .map(|offset| actor_kind_insert + offset)
            .expect("missing invalid_route error_class injection");
        let route_eval = source[error_class_insert..]
            .find("crate::invariants::evaluate_invariant_gate(\"route\", &state, &ws)")
            .map(|offset| error_class_insert + offset)
            .expect("missing route-gate invariant evaluation");

        assert!(
            invalid_route_count < actor_kind_insert
                && actor_kind_insert < error_class_insert
                && error_class_insert < route_eval,
            "INV-171c039a wiring must inject the exact orchestrator invalid_route tuple before route-gate evaluation"
        );
    }

    #[test]
    fn submit_ack_tab_mismatch_is_canonicalized_before_turn_registration() {
        let source = include_str!("app_submit_completion.rs");
        let mismatch = source
            .find("log_submit_ack_tab_mismatch(ctx, lane_id, active_tab, tab_id);")
            .expect("missing submit ack mismatch log");
        let rebind = source[mismatch..]
            .find("ControlEvent::ExecutorSubmitAckTabRebound {")
            .map(|offset| mismatch + offset)
            .expect("missing canonical submit ack tab rebound");
        let register = source[rebind..]
            .find("register_submitted_executor_turn(")
            .map(|offset| rebind + offset)
            .expect("missing turn registration after submit ack handling");

        assert!(
            mismatch < rebind && rebind < register,
            "submit ack mismatch must emit a canonical tab rebound before turn registration"
        );
    }

    #[test]
    fn executor_bootstrap_with_ready_tasks_wakes_planner_before_silent_idle() {
        let source = include_str!("app_submit_completion.rs");
        let ready_guard = source
            .find("let ready_count = if ready_tasks_text == \"(no ready tasks)\"")
            .expect("missing ready task guard");
        let bootstrap_guard = source[ready_guard..]
            .find(
                "executor bootstrap: ready tasks exist but no lane work is seeded; waking planner",
            )
            .map(|offset| ready_guard + offset)
            .expect("missing clean-start executor bootstrap guard");
        let planner_wake = source[bootstrap_guard..]
            .find("writer.apply(ControlEvent::PlannerPendingSet { pending: true });")
            .map(|offset| bootstrap_guard + offset)
            .expect("missing planner wake after executor bootstrap guard");
        let lane_claim = source[planner_wake..]
            .find("if let Some(job) = claim_executor_submit(writer, lane) {")
            .map(|offset| planner_wake + offset)
            .expect("missing executor lane claim after bootstrap guard");

        assert!(
            ready_guard < bootstrap_guard && bootstrap_guard < planner_wake && planner_wake < lane_claim,
            "executor bootstrap guard must wake planner before lane claim to avoid clean-start idle stalls"
        );
    }

    #[test]
    fn invariant_id_is_extracted_from_gate_reason() {
        let reason = "invariant gate blocked role `executor`: Action targeted a path that does not exist — plan is referencing a target that has not been created yet [id=INV-47232c36]";
        assert_eq!(invariant_id_from_reason(reason), Some("INV-47232c36"));
    }

    #[test]
    fn route_gate_blocker_message_is_structured_for_planner_repair() {
        let reason = "invariant gate blocked role `executor`: Action targeted a path that does not exist — plan is referencing a target that has not been created yet [id=INV-47232c36]";
        let message = route_gate_blocker_message(reason);
        assert_eq!(
            message.get("action").and_then(|v| v.as_str()),
            Some("message")
        );
        assert_eq!(message.get("to").and_then(|v| v.as_str()), Some("planner"));
        assert_eq!(
            message.get("type").and_then(|v| v.as_str()),
            Some("blocker")
        );
        assert_eq!(
            message.get("status").and_then(|v| v.as_str()),
            Some("blocked")
        );
        let payload = message.get("payload").expect("payload");
        assert_eq!(
            payload.get("summary").and_then(|v| v.as_str()),
            Some("Executor dispatch blocked by enforced invariant INV-47232c36")
        );
        assert_eq!(
            payload.get("blocker").and_then(|v| v.as_str()),
            Some("Plan references a path that does not exist yet")
        );
        assert_eq!(
            payload.get("evidence").and_then(|v| v.as_str()),
            Some(reason)
        );
    }

    #[test]
    fn chromium_transport_errors_are_detected_for_local_blocker_synthesis() {
        assert!(is_chromium_transport_error(
            "chromium: early transport failure (heartbeat_after_user_echo_before_turn_complete) (tab=1 turn=2)"
        ));
        assert!(is_chromium_transport_error(
            "chromium: timeout waiting for SUBMIT_ACK (tab=1 turn=2)"
        ));
        assert!(!is_chromium_transport_error("schema validation failed"));
    }

    #[test]
    fn local_transport_blocker_message_routes_without_extra_llm_turn() {
        let action = local_transport_blocker_message(
            "planner",
            "chromium: early transport failure (heartbeat_after_user_echo_before_turn_complete) (tab=633187572 turn=4)",
            "Planner task context",
        );
        assert_eq!(
            action.get("action").and_then(|v| v.as_str()),
            Some("message")
        );
        assert_eq!(action.get("from").and_then(|v| v.as_str()), Some("planner"));
        assert_eq!(action.get("to").and_then(|v| v.as_str()), Some("executor"));
        assert_eq!(action.get("type").and_then(|v| v.as_str()), Some("blocker"));
        let payload = action.get("payload").expect("payload");
        assert_eq!(
            payload.get("blocker").and_then(|v| v.as_str()),
            Some("Chromium transport/runtime failure prevented a usable assistant completion")
        );
        assert!(payload
            .get("evidence")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("heartbeat_after_user_echo_before_turn_complete"));
    }

    #[test]
    fn inbound_message_from_user_detects_external_user_sender() {
        let inbound = r#"{"action":"message","from":"user","to":"solo","type":"handoff","status":"ready","payload":{"summary":"hello"}}"#;
        assert!(inbound_message_from_user(inbound));
    }

    #[test]
    fn inbound_message_from_user_rejects_non_user_sender() {
        let inbound = r#"{"action":"message","from":"planner","to":"solo","type":"handoff","status":"ready","payload":{"summary":"hello"}}"#;
        assert!(!inbound_message_from_user(inbound));
    }

    #[test]
    fn inbound_message_without_writer_ignores_projection_without_canonical_tlog_record() {
        let _guard = global_state_lock().lock().expect("lock");
        let workspace = temp_workspace("projection-only-inbound");
        let state_dir = workspace.join("agent_state");
        fs::create_dir_all(&state_dir).unwrap();
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());

        fs::write(
            state_dir.join("last_message_to_planner.json"),
            serde_json::to_string_pretty(&json!({
                "action": "message",
                "from": "executor",
                "to": "planner",
                "payload": {"summary": "projection only"}
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(take_inbound_message_without_writer("planner").is_none());
    }

    #[test]
    fn external_user_message_without_writer_ignores_projection_without_canonical_tlog_record() {
        let _guard = global_state_lock().lock().expect("lock");
        let workspace = temp_workspace("projection-only-external");
        let state_dir = workspace.join("agent_state");
        fs::create_dir_all(&state_dir).unwrap();
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());

        fs::write(
            state_dir.join("external_user_message_to_executor.json"),
            serde_json::to_string_pretty(&json!({
                "kind": "external_user_message",
                "from": "user",
                "to": "executor",
                "message": "projection only"
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(take_external_user_message_without_writer("executor").is_none());
    }

    #[test]
    fn external_user_message_without_writer_reads_canonical_tlog_when_projection_missing() {
        let _guard = global_state_lock().lock().expect("lock");
        let workspace = temp_workspace("canonical-external");
        let state_dir = workspace.join("agent_state");
        fs::create_dir_all(&state_dir).unwrap();
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());

        let message = serde_json::to_string_pretty(&json!({
            "kind": "external_user_message",
            "from": "user",
            "to": "executor",
            "message": "canonical event"
        }))
        .unwrap();
        let signature = artifact_write_signature(&[
            "external_user_message",
            "executor",
            &message.len().to_string(),
            message.as_str(),
        ]);
        record_effect_for_workspace(
            &workspace,
            EffectEvent::ExternalUserMessageRecorded {
                to_role: "executor".to_string(),
                message: message.clone(),
                signature,
            },
        )
        .unwrap();

        let recovered = take_external_user_message_without_writer("executor").unwrap();
        assert!(recovered.contains("canonical event"));
    }

    #[test]
    fn canonical_inbound_message_skips_historical_replay_when_latest_consumed() {
        let _guard = global_state_lock().lock().expect("lock");
        let workspace = temp_workspace("canonical-inbound-latest-only");
        let state_dir = workspace.join("agent_state");
        fs::create_dir_all(&state_dir).unwrap();
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());

        record_effect_for_workspace(
            &workspace,
            EffectEvent::InboundMessageRecorded {
                from_role: "planner".to_string(),
                to_role: "executor".to_string(),
                message: "{\"payload\":{\"summary\":\"old\"}}".to_string(),
                signature: "sig-old".to_string(),
            },
        )
        .unwrap();
        record_effect_for_workspace(
            &workspace,
            EffectEvent::InboundMessageRecorded {
                from_role: "planner".to_string(),
                to_role: "executor".to_string(),
                message: "{\"payload\":{\"summary\":\"new\"}}".to_string(),
                signature: "sig-new".to_string(),
            },
        )
        .unwrap();

        let mut state = SystemState::new(&[], 0);
        state
            .inbound_message_signatures
            .insert("executor".to_string(), "sig-new".to_string());

        assert!(canonical_recorded_message_from_tlog(
            &state_dir,
            &state,
            "executor",
            RecordedMessageKind::Inbound,
        )
        .is_none());
    }

    #[test]
    fn canonical_wake_signals_read_from_state_not_tlog() {
        // WakeSignalQueued populates wake_signals_pending in SystemState.
        // collect_wake_signal_inputs reads directly from state — no tlog scan.
        // Consumed signals are absent; pending ones are present.
        let state_dir = std::path::PathBuf::from("/tmp/wake-state-test");
        fs::create_dir_all(&state_dir).unwrap();

        let mut state = SystemState::new(&[], 0);
        // Pending signal for planner
        state
            .wake_signals_pending
            .insert("planner".to_string(), (1000, "sig-pending".to_string()));
        // Already consumed signal for executor (only in wake_signal_signatures, not pending)
        state
            .wake_signal_signatures
            .insert("executor".to_string(), "sig-consumed".to_string());

        let (inputs, sig_map) = collect_wake_signal_inputs(&state);
        // Planner signal present
        assert!(
            inputs.iter().any(|i| i.role == "planner"),
            "planner should be pending"
        );
        assert_eq!(
            sig_map.get("planner").map(String::as_str),
            Some("sig-pending")
        );
        // Executor not present (consumed, not pending)
        assert!(
            !inputs.iter().any(|i| i.role == "executor"),
            "executor should not appear"
        );
    }

    #[test]
    fn workspace_artifact_baseline_creates_missing_planner_projection_inputs() {
        let workspace = temp_workspace("baseline-create");
        let planner_projection_path = workspace.join("agent_state/default/planner-default.json");

        let created = ensure_workspace_artifact_baseline(&workspace, &planner_projection_path)
            .expect("bootstrap baseline");

        assert!(created.iter().any(|p| p == VIOLATIONS_FILE));
        assert!(created.iter().any(|p| p == MASTER_PLAN_FILE));
        assert!(created.iter().any(|p| p == "agent_state/blockers.json"));
        assert!(created.iter().any(|p| p == "agent_state/tlog.ndjson"));
        assert!(created.iter().any(|p| p == "agent_state/lessons.json"));
        assert!(workspace.join(VIOLATIONS_FILE).exists());
        assert!(workspace.join(ISSUES_FILE).exists());
        assert!(workspace.join(MASTER_PLAN_FILE).exists());
        assert!(workspace.join("agent_state/blockers.json").exists());
        assert!(workspace.join("agent_state/tlog.ndjson").exists());
        assert!(workspace.join("agent_state/lessons.json").exists());
        assert!(planner_projection_path.exists());

        let violations = fs::read_to_string(workspace.join(VIOLATIONS_FILE)).unwrap();
        assert!(violations.contains("\"status\": \"ok\""));

        let plan = fs::read_to_string(workspace.join(MASTER_PLAN_FILE)).unwrap();
        assert!(plan.contains("\"ready_window\": []"));

        let blockers = fs::read_to_string(workspace.join("agent_state/blockers.json")).unwrap();
        assert!(blockers.contains("\"blockers\": []"));

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn workspace_artifact_baseline_preserves_existing_nonempty_files() {
        let workspace = temp_workspace("baseline-preserve");
        fs::create_dir_all(workspace.join("agent_state")).unwrap();
        fs::write(
            workspace.join(VIOLATIONS_FILE),
            "{\n  \"status\": \"failed\",\n  \"summary\": \"keep\",\n  \"violations\": []\n}\n",
        )
        .unwrap();
        let planner_projection_path = workspace.join("agent_state/default/planner-default.json");

        let created = ensure_workspace_artifact_baseline(&workspace, &planner_projection_path)
            .expect("bootstrap baseline");

        assert!(!created.iter().any(|p| p == VIOLATIONS_FILE));
        let violations = fs::read_to_string(workspace.join(VIOLATIONS_FILE)).unwrap();
        assert!(violations.contains("\"summary\": \"keep\""));

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn workspace_artifact_baseline_migrates_legacy_root_plan_and_violations() {
        let workspace = temp_workspace("baseline-migrate-legacy");
        fs::create_dir_all(workspace.join("agent_state")).unwrap();
        fs::write(
            workspace.join("PLAN.json"),
            "{\"version\":2,\"tasks\":[{\"id\":\"T1\",\"status\":\"ready\"}]}",
        )
        .unwrap();
        fs::write(
            workspace.join("VIOLATIONS.json"),
            "{\"status\":\"failed\",\"summary\":\"legacy\",\"violations\":[]}",
        )
        .unwrap();
        let planner_projection_path = workspace.join("agent_state/default/planner-default.json");

        let created = ensure_workspace_artifact_baseline(&workspace, &planner_projection_path)
            .expect("bootstrap baseline");

        assert!(created.iter().any(|p| p == MASTER_PLAN_FILE));
        assert!(created.iter().any(|p| p == VIOLATIONS_FILE));
        assert!(!workspace.join("PLAN.json").exists());
        assert!(!workspace.join("VIOLATIONS.json").exists());
        assert!(workspace.join(MASTER_PLAN_FILE).exists());
        assert!(workspace.join(VIOLATIONS_FILE).exists());
        let plan = fs::read_to_string(workspace.join(MASTER_PLAN_FILE)).unwrap();
        assert!(plan.contains("\"T1\""));
        let violations: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(workspace.join(VIOLATIONS_FILE)).unwrap())
                .unwrap();
        assert_eq!(violations["summary"].as_str(), Some("legacy"));

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn build_agent_prompt_includes_role_schema_on_nonzero_steps_when_enabled() {
        let (schema0, prompt0) = super::build_agent_prompt(
            "planner",
            true,
            0,
            "INIT",
            "SYSTEM",
            None,
            None,
            None,
            None,
            &ActionProvenance::default(),
            0,
            None,
        );
        assert_eq!(schema0, "SYSTEM");
        assert!(
            prompt0.starts_with("TAB_ID: pending\nTURN_ID: pending\nAGENT_TYPE: PLANNER\n\n"),
            "initial prompt must include the identity banner"
        );
        assert!(prompt0.ends_with("INIT"));

        let (schema1, prompt1) = super::build_agent_prompt(
            "planner",
            true,
            1,
            "INIT",
            "SYSTEM",
            Some("LAST_RESULT"),
            None,
            None,
            Some("read_file"),
            &ActionProvenance::default(),
            1,
            None,
        );
        assert_eq!(schema1, "SYSTEM");
        assert!(
            prompt1.contains("LAST_RESULT"),
            "prompt must include last result"
        );

        let (schema_disabled, _) = super::build_agent_prompt(
            "planner",
            false,
            1,
            "INIT",
            "SYSTEM",
            Some("LAST_RESULT"),
            None,
            None,
            None,
            &ActionProvenance::default(),
            1,
            None,
        );
        assert!(
            schema_disabled.trim().is_empty(),
            "role_schema must be empty when disabled"
        );
    }

    #[test]
    fn stateful_endpoints_only_send_system_prompt_on_first_step() {
        assert!(super::should_send_system_prompt(true, true, 0));
        assert!(!super::should_send_system_prompt(true, true, 1));
        assert!(super::should_send_system_prompt(true, false, 1));
        assert!(!super::should_send_system_prompt(false, true, 0));
    }

    #[test]
    fn restart_resume_prompt_is_a_short_continuation_prompt() {
        let resume = super::PostRestartResult {
            role: "planner".to_string(),
            action: "read_file".to_string(),
            result: "file contents".to_string(),
            step: 4,
            tab_id: Some(433977893),
            turn_id: Some(1),
            endpoint_id: "mini_planner_chatgpt".to_string(),
            restart_kind: "process_restart".to_string(),
            signature: "test-signature".to_string(),
        };
        let prompt = super::build_restart_resume_prompt("planner", &resume);
        assert!(prompt.contains("SYSTEM RESTART RESUME"));
        assert!(prompt.contains("Resume role: planner"));
        assert!(prompt.contains("Restart kind: process_restart"));
        assert!(prompt.contains("Endpoint: mini_planner_chatgpt"));
        assert!(prompt.contains("Last completed action: `read_file` (step 4)"));
        assert!(prompt.contains("Continue from the last completed action result below."));
        assert!(!prompt.contains("canonical law"));
    }

    #[test]
    fn verifier_confirmed_rejects_when_plan_has_incomplete_tasks() {
        let reason = r#"{"verified":true,"summary":"ok"}"#;
        let plan = r#"{
          "version": 1,
          "tasks": [
            {"id": "T1", "status": "ready"},
            {"id": "T2", "status": "done"}
          ]
        }"#;
        assert!(plan_has_incomplete_tasks(plan));
        assert!(!verifier_confirmed_with_plan_text(reason, plan));
    }

    #[test]
    fn verifier_confirmed_accepts_only_verified_when_plan_is_done() {
        let verified = r#"{"verified":true,"summary":"ok"}"#;
        let unverified = r#"{"verified":false,"summary":"blocked"}"#;
        let plan = r#"{
          "version": 1,
          "tasks": [
            {"id": "T1", "status": "done"},
            {"id": "T2", "status": "done"}
          ]
        }"#;
        assert!(!plan_has_incomplete_tasks(plan));
        assert!(verifier_confirmed_with_plan_text(verified, plan));
        assert!(!verifier_confirmed_with_plan_text(unverified, plan));
    }

    #[test]
    fn executor_step_limit_feedback_prefers_plan_status_update() {
        let feedback = executor_step_limit_feedback();
        assert!(feedback.contains("\"action\": \"plan\""));
        assert!(feedback.contains("\"op\": \"set_task_status\""));
        assert!(feedback.contains("\"status\": \"done\" | \"in_progress\""));
        assert!(feedback.contains("Only if blocked/unresolvable"));
        assert!(feedback.contains("\"type\": \"blocker\""));
        assert!(feedback.contains("\"required_action\": \"What planner should do next\""));
    }

    #[test]
    fn actionable_objectives_ignore_deferred_or_blocked_and_done() {
        let objectives = r#"{
          "version": 1,
          "objectives": [
            {"id":"o1","status":"done"},
            {"id":"o2","status":"deferred"},
            {"id":"o3","status":"blocked"}
          ]
        }"#;
        assert!(!has_actionable_objectives(objectives));
    }

    #[test]
    fn actionable_objectives_detect_active_entries() {
        let objectives = r#"{
          "version": 1,
          "objectives": [
            {"id":"o1","status":"done"},
            {"id":"o2","status":"active"}
          ]
        }"#;
        assert!(has_actionable_objectives(objectives));
    }

    #[test]
    fn solo_complete_rejected_when_objectives_actionable_and_plan_done() {
        let action = json!({
            "action": "message",
            "status": "complete"
        });
        let objectives = r#"{
          "version": 1,
          "objectives": [
            {"id":"o1","status":"active"}
          ]
        }"#;
        let plan = r#"{
          "version": 1,
          "tasks": [
            {"id":"T1","status":"done"}
          ]
        }"#;
        assert!(should_reject_solo_self_complete(&action, objectives, plan));
    }

    #[test]
    fn solo_complete_not_rejected_when_plan_has_incomplete_tasks() {
        let action = json!({
            "action": "message",
            "status": "complete"
        });
        let objectives = r#"{
          "version": 1,
          "objectives": [
            {"id":"o1","status":"active"}
          ]
        }"#;
        let plan = r#"{
          "version": 1,
          "tasks": [
            {"id":"T1","status":"todo"}
          ]
        }"#;
        assert!(!should_reject_solo_self_complete(&action, objectives, plan));
    }

    #[test]
    fn system_state_lane_accessors_return_correct_defaults_and_values() {
        let mut state = SystemState::new(&[7], 1);

        assert!(!state.lane_in_flight(7));
        assert!(!state.lane_submit_active(7));
        assert_eq!(state.lane_next_submit_ms(7), 0);
        assert_eq!(state.lane_steps_used_count(7), 0);
        assert_eq!(state.lane_active_tab_id(7), None);

        // Defaults for absent lanes
        assert!(!state.lane_in_flight(99));
        assert!(!state.lane_submit_active(99));
        assert_eq!(state.lane_next_submit_ms(99), 0);
        assert_eq!(state.lane_steps_used_count(99), 0);
        assert_eq!(state.lane_active_tab_id(99), None);

        state.lane_prompt_in_flight.insert(7, true);
        state.lane_submit_in_flight.insert(7, true);
        state.lane_next_submit_at_ms.insert(7, 42);
        state.lane_steps_used.insert(7, 3);
        state.lane_active_tab.insert(7, 99);

        assert!(state.lane_in_flight(7));
        assert!(state.lane_submit_active(7));
        assert_eq!(state.lane_next_submit_ms(7), 42);
        assert_eq!(state.lane_steps_used_count(7), 3);
        assert_eq!(state.lane_active_tab_id(7), Some(99));
    }

    #[test]
    fn planner_message_ready_to_executor_allows_dispatch() {
        let completion = super::AgentCompletion::MessageAction {
            action: json!({
                "action": "message",
                "to": "executor",
                "status": "ready"
            }),
            summary: "ok".to_string(),
        };
        assert!(planner_completion_allows_executor_dispatch(&completion));
        assert_eq!(
            classify_planner_action_result_class(&completion),
            super::PlannerActionResultClass::ReadyHandoff
        );
    }

    #[test]
    fn planner_message_blocked_keeps_planner_phase() {
        let completion = super::AgentCompletion::MessageAction {
            action: json!({
                "action": "message",
                "to": "executor",
                "status": "blocked"
            }),
            summary: "blocked".to_string(),
        };
        assert!(!planner_completion_allows_executor_dispatch(&completion));
        assert_eq!(
            classify_planner_action_result_class(&completion),
            super::PlannerActionResultClass::BlockedHandoff
        );
    }

    #[test]
    fn planner_summary_ready_task_dispatch_allows_executor() {
        let completion = super::AgentCompletion::Summary(
            "plan ok — ready task `task_1` dispatched\nplan_path: agent_state/PLAN.json".to_string(),
        );
        assert!(planner_completion_allows_executor_dispatch(&completion));
        assert_eq!(
            classify_planner_action_result_class(&completion),
            super::PlannerActionResultClass::ReadyHandoff
        );
    }

    #[test]
    fn stale_executor_claim_detects_busy_lane_without_live_work() {
        let mut state = SystemState::new(&[0], 1);
        state.lanes.get_mut(&0).unwrap().in_progress_by = Some("executor_pool".to_string());
        state.lane_active_tab.insert(0, 42);

        assert!(lane_has_stale_executor_claim(&state, 0));

        state.lane_submit_in_flight.insert(0, true);
        assert!(!lane_has_stale_executor_claim(&state, 0));
    }

    #[test]
    fn action_retry_fingerprint_ignores_volatile_fields() {
        let a = json!({
            "action": "plan",
            "op": "set_task_status",
            "task_id": "T1",
            "status": "done",
            "observation": "first",
            "rationale": "r1",
            "question": "q1",
            "predicted_next_actions": [{"action":"read_file","intent":"next"}],
            "command_id": "solo:solo:0001:1"
        });
        let b = json!({
            "action": "plan",
            "op": "set_task_status",
            "task_id": "T1",
            "status": "done",
            "observation": "second",
            "rationale": "r2",
            "question": "q2",
            "predicted_next_actions": [{"action":"message","intent":"different"}],
            "command_id": "solo:solo:0002:2"
        });

        assert_eq!(action_retry_fingerprint(&a), action_retry_fingerprint(&b));
    }
}
