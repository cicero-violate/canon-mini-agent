use anyhow::Result;
use canon_llm::config::LlmEndpoint;
use serde_json::{json, Value};
use std::path::Path;

use crate::logging::{append_llm_completion_log, log_error_event};
use crate::tools::execute_logged_action;

pub(crate) fn process_action_and_execute(
    role: &str,
    prompt_kind: &str,
    endpoint: &LlmEndpoint,
    workspace: &Path,
    step: usize,
    command_id: &str,
    action: &Value,
    check_on_done: bool,
) -> Result<(bool, String)> {
    if let Err(log_err) = append_llm_completion_log(role, endpoint, step, command_id, action) {
        eprintln!("[{role}] step={} completion_log_error: {log_err}", step);
        log_error_event(
            role,
            "completion_log",
            Some(step),
            &format!("completion_log_error: {log_err}"),
            Some(json!({ "command_id": command_id, "prompt_kind": prompt_kind })),
        );
    }
    execute_logged_action(
        role,
        prompt_kind,
        endpoint,
        workspace,
        step,
        command_id,
        action,
        check_on_done,
    )
}
