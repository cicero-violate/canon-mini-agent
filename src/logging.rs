use anyhow::{Context, Result};
use canon_llm::config::LlmEndpoint;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::constants::MAX_SNIPPET;
use crate::prompts::{action_observation, action_rationale, parse_actions, truncate};

struct LogPaths {
    action_log: PathBuf,
    secondary_log: PathBuf,
}
static LOG_PATHS: OnceLock<LogPaths> = OnceLock::new();

pub fn init_log_paths(prefix: &str) {
    let base = std::path::Path::new(crate::constants::agent_state_dir()).join(prefix);
    let _ = std::fs::create_dir_all(&base);
    let _ = LOG_PATHS.set(LogPaths {
        action_log: base.join("actions.jsonl"),
        secondary_log: base.join("log.jsonl"),
    });
}

fn log_paths() -> Result<&'static LogPaths> {
    LOG_PATHS.get().ok_or_else(|| anyhow::anyhow!("log paths not initialized"))
}

fn patch_summary_path(patch: &str) -> Option<&str> {
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("*** Update File:").or_else(|| line.strip_prefix("*** Add File:")) {
            return Some(rest.trim());
        }
    }
    None
}

fn action_command_summary(action: &Value) -> String {
    let kind = action.get("action").and_then(|v| v.as_str()).unwrap_or("unknown");
    match kind {
        "run_command" => action.get("cmd").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        "python" => {
            let code = action.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let first = code.lines().next().unwrap_or("");
            format!("python: {}", truncate(first, 160))
        }
        "read_file" => {
            let path = action.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let line = action.get("line").and_then(|v| v.as_u64());
            match line {
                Some(n) => format!("read_file {}:{}", path, n),
                None => format!("read_file {}", path),
            }
        }
        "list_dir" => format!("list_dir {}", action.get("path").and_then(|v| v.as_str()).unwrap_or("")),
        "apply_patch" => {
            let patch = action.get("patch").and_then(|v| v.as_str()).unwrap_or("");
            patch_summary_path(patch)
                .map(|path| format!("apply_patch {}", path))
                .unwrap_or_else(|| "apply_patch".to_string())
        }
        "message" => {
            let status = action.get("status").and_then(|v| v.as_str()).unwrap_or("");
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

fn parse_action_from_text(text: &str) -> Option<Value> {
    parse_actions(text).ok().and_then(|actions| actions.into_iter().next())
}

fn observation_and_rationale_from_text(text: &str) -> (Option<String>, Option<String>) {
    let Some(action) = parse_action_from_text(text) else {
        return (None, None);
    };
    (
        action_observation(&action).map(str::to_string),
        action_rationale(&action).map(str::to_string),
    )
}

fn append_record_to_path(path: &PathBuf, record: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    writeln!(file, "{}", serde_json::to_string(record)?)
        .with_context(|| format!("failed to append {}", path.display()))?;
    Ok(())
}

pub(crate) fn append_action_log_record(record: &Value) -> Result<()> {
    static LOG_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = LOG_MUTEX.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().expect("action log mutex poisoned");

    let primary = log_paths()?.action_log.clone();
    append_record_to_path(&primary, record)?;
    Ok(())
}

pub(crate) fn make_command_id(role: &str, prompt_kind: &str, step: usize) -> String {
    format!("{}:{}:{}:{}", role, prompt_kind, step, now_ms())
}

fn compact_json(value: Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::String(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(Value::String(text))
            }
        }
        Value::Array(items) => {
            let items = items.into_iter().filter_map(compact_json).collect::<Vec<_>>();
            if items.is_empty() {
                None
            } else {
                Some(Value::Array(items))
            }
        }
        Value::Object(fields) => {
            let mut out = serde_json::Map::new();
            for (key, value) in fields {
                if let Some(value) = compact_json(value) {
                    out.insert(key, value);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Value::Object(out))
            }
        }
        other => Some(other),
    }
}

pub(crate) fn compact_log_record(
    kind: &str,
    phase: &str,
    actor: Option<&str>,
    lane: Option<&str>,
    endpoint_id: Option<&str>,
    step: Option<usize>,
    turn_id: Option<u64>,
    command_id: Option<&str>,
    op: Option<Value>,
    ok: Option<bool>,
    observation: Option<String>,
    rationale: Option<String>,
    text: Option<String>,
    meta: Option<Value>,
) -> Value {
    let mut record = serde_json::Map::new();
    record.insert(
        "ts_ms".to_string(),
        json!(canon_llm::endpoint_worker::tab_manager_now_ms()),
    );
    record.insert("kind".to_string(), json!(kind));
    record.insert("phase".to_string(), json!(phase));

    if let Some(value) = actor.and_then(|value| compact_json(json!(value))) {
        record.insert("actor".to_string(), value);
    }
    if let Some(value) = lane.and_then(|value| compact_json(json!(value))) {
        record.insert("lane".to_string(), value);
    }
    if let Some(value) = endpoint_id.and_then(|value| compact_json(json!(value))) {
        record.insert("endpoint_id".to_string(), value);
    }
    if let Some(value) = step.and_then(|value| compact_json(json!(value))) {
        record.insert("step".to_string(), value);
    }
    if let Some(value) = turn_id.and_then(|value| compact_json(json!(value))) {
        record.insert("turn_id".to_string(), value);
    }
    if let Some(value) = command_id.and_then(|value| compact_json(json!(value))) {
        record.insert("command_id".to_string(), value);
    }
    if let Some(value) = op.and_then(compact_json) {
        record.insert("op".to_string(), value);
    }
    if let Some(value) = ok.and_then(|value| compact_json(json!(value))) {
        record.insert("ok".to_string(), value);
    }
    if let Some(value) = observation.and_then(|value| compact_json(json!(truncate(&value, MAX_SNIPPET)))) {
        record.insert("observation".to_string(), value);
    }
    if let Some(value) = rationale.and_then(|value| compact_json(json!(truncate(&value, MAX_SNIPPET)))) {
        record.insert("rationale".to_string(), value);
    }
    if let Some(value) = text.and_then(|value| compact_json(json!(truncate(&value, MAX_SNIPPET)))) {
        record.insert("text".to_string(), value);
    }
    if let Some(value) = meta.and_then(compact_json) {
        record.insert("meta".to_string(), value);
    }

    Value::Object(record)
}

fn action_op(action: &Value) -> Option<Value> {
    let name = action.get("action").and_then(|v| v.as_str())?;
    let summary = action_command_summary(action);
    Some(json!({
        "name": name,
        "summary": summary,
    }))
}

fn filtered_payload_meta(payload: &Value) -> Option<Value> {
    let object = payload.as_object()?;
    let mut meta = serde_json::Map::new();
    for (key, value) in object {
        if matches!(
            key.as_str(),
            "role"
                | "prompt_kind"
                | "step"
                | "endpoint_id"
                | "lane_name"
                | "turn_id"
                | "command_id"
                | "action"
                | "proposed_action"
                | "command_used"
                | "proposed_command"
                | "success"
                | "summary"
                | "result"
                | "reason"
        ) {
            continue;
        }
        if let Some(value) = compact_json(value.clone()) {
            meta.insert(key.clone(), value);
        }
    }
    if meta.is_empty() {
        None
    } else {
        Some(Value::Object(meta))
    }
}

fn inject_action_fields(record: &mut Value, action: &Value) {
    let Some(obj) = record.as_object_mut() else {
        return;
    };
    let mut insert_if_missing = |key: &str, value: Option<Value>| {
        if obj.contains_key(key) {
            return;
        }
        if let Some(value) = value.and_then(compact_json) {
            obj.insert(key.to_string(), value);
        }
    };
    insert_if_missing("action", action.get("action").cloned());
    insert_if_missing("path", action.get("path").cloned());
    insert_if_missing("line", action.get("line").cloned());
}

fn append_secondary_action_log(role: &str, action: &Value) -> Result<()> {
    static SECONDARY_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = SECONDARY_MUTEX.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().expect("secondary action log mutex poisoned");

    let mut record = serde_json::Map::new();
    record.insert("agent_role".to_string(), Value::String(role.to_string()));
    record.insert(
        "timestamp".to_string(),
        json!(canon_llm::endpoint_worker::tab_manager_now_ms()),
    );
    if let Some(value) = action.get("observation").cloned().and_then(compact_json) {
        record.insert("observation".to_string(), value);
    }
    if let Some(value) = action.get("action").cloned().and_then(compact_json) {
        record.insert("action".to_string(), value);
    }
    if let Some(value) = action.get("path").cloned().and_then(compact_json) {
        record.insert("path".to_string(), value);
    }
    if let Some(value) = action.get("line").cloned().and_then(compact_json) {
        record.insert("line".to_string(), value);
    }
    if let Some(value) = action.get("rationale").cloned().and_then(compact_json) {
        record.insert("rationale".to_string(), value);
    }
    if record.is_empty() {
        return Ok(());
    }
    let path = log_paths()?.secondary_log.clone();
    append_record_to_path(&path, &Value::Object(record))
}

pub(crate) fn append_message_log(
    role: &str,
    endpoint: &LlmEndpoint,
    _prompt_kind: &str,
    step: usize,
    command_id: &str,
    record_type: &str,
    payload: Value,
) -> Result<()> {
    let (kind, phase) = match record_type {
        "llm_request" => ("llm", "request"),
        "llm_response" => ("llm", "response"),
        "llm_submit_ack" => ("llm", "ack"),
        "llm_error" => ("llm", "error"),
        "llm_parse_error" => ("llm", "parse_error"),
        other => ("log", other),
    };
    let text = payload
        .get("prompt")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("raw").and_then(|v| v.as_str()))
        .or_else(|| payload.get("error").and_then(|v| v.as_str()))
        .map(str::to_string);
    let (observation, rationale) = text
        .as_deref()
        .map(observation_and_rationale_from_text)
        .unwrap_or((None, None));
    let meta = filtered_payload_meta(&payload);
    let turn_id = payload.get("turn_id").and_then(|v| v.as_u64());
    let record = compact_log_record(
        kind,
        phase,
        Some(role),
        None,
        Some(&endpoint.id),
        Some(step),
        turn_id,
        Some(command_id),
        None,
        None,
        observation,
        rationale,
        text,
        meta,
    );
    append_action_log_record(&record)
}

pub(crate) fn log_message_event(
    role: &str,
    endpoint: &LlmEndpoint,
    prompt_kind: &str,
    step: usize,
    command_id: &str,
    event: &str,
    payload: Value,
) {
    if let Err(err) = append_message_log(
        role,
        endpoint,
        prompt_kind,
        step,
        command_id,
        event,
        payload,
    ) {
        eprintln!("[{role}] step={} action_log_error: {err}", step);
    }
}

pub(crate) fn append_action_log(role: &str, endpoint: &LlmEndpoint, _prompt_kind: &str, step: usize, command_id: &str, action: &Value) -> Result<()> {
    let observation = action_observation(action).unwrap_or("");
    let rationale = action_rationale(action).unwrap_or("");
    let text = match (observation.is_empty(), rationale.is_empty()) {
        (false, false) => Some(format!("{} | {}", observation, rationale)),
        (false, true) => Some(observation.to_string()),
        (true, false) => Some(rationale.to_string()),
        (true, true) => None,
    };
    let mut record = compact_log_record(
        "tool",
        "request",
        Some(role),
        None,
        Some(&endpoint.id),
        Some(step),
        None,
        Some(command_id),
        action_op(action),
        None,
        action_observation(action).map(str::to_string),
        action_rationale(action).map(str::to_string),
        text,
        None,
    );
    inject_action_fields(&mut record, action);
    append_action_log_record(&record)?;
    append_secondary_action_log(role, action)
}

pub(crate) fn log_action_event(
    role: &str,
    endpoint: &LlmEndpoint,
    prompt_kind: &str,
    step: usize,
    command_id: &str,
    action: &Value,
) {
    if let Err(e) = append_action_log(role, endpoint, prompt_kind, step, command_id, action) {
        eprintln!("[{role}] step={} action_log_error: {e}", step);
    }
}

pub(crate) fn append_action_result_log(
    role: &str,
    endpoint: &LlmEndpoint,
    _prompt_kind: &str,
    step: usize,
    command_id: &str,
    action: &Value,
    success: bool,
    result_text: &str,
) -> Result<()> {
    let mut record = compact_log_record(
        "tool",
        "result",
        Some(role),
        None,
        Some(&endpoint.id),
        Some(step),
        None,
        Some(command_id),
        action_op(action),
        Some(success),
        action_observation(action).map(str::to_string),
        action_rationale(action).map(str::to_string),
        Some(result_text.to_string()),
        None,
    );
    inject_action_fields(&mut record, action);
    append_action_log_record(&record)
}

pub(crate) fn log_action_result(
    role: &str,
    endpoint: &LlmEndpoint,
    prompt_kind: &str,
    step: usize,
    command_id: &str,
    action: &Value,
    success: bool,
    output: &str,
) {
    if let Err(e) = append_action_result_log(
        role,
        endpoint,
        prompt_kind,
        step,
        command_id,
        action,
        success,
        output,
    ) {
        eprintln!("[{role}] step={} action_result_log_error: {e}", step);
    }
}

pub(crate) fn append_llm_completion_log(
    role: &str,
    endpoint: &LlmEndpoint,
    step: usize,
    command_id: &str,
    action: &Value,
) -> Result<()> {
    let text = serde_json::to_string(action).ok();
    let mut record = compact_log_record(
        "llm",
        "completion",
        Some(role),
        None,
        Some(&endpoint.id),
        Some(step),
        None,
        Some(command_id),
        action_op(action),
        None,
        action_observation(action).map(str::to_string),
        action_rationale(action).map(str::to_string),
        text,
        None,
    );
    inject_action_fields(&mut record, action);
    append_action_log_record(&record)
}

pub(crate) fn append_orchestration_trace(event: &str, payload: Value) {
    let actor = payload
        .get("role")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("from").and_then(|v| v.as_str()));
    let lane = payload.get("lane_name").and_then(|v| v.as_str());
    let endpoint_id = payload.get("endpoint_id").and_then(|v| v.as_str());
    let step = payload
        .get("step")
        .and_then(|v| v.as_u64())
        .and_then(|v| usize::try_from(v).ok());
    let turn_id = payload.get("turn_id").and_then(|v| v.as_u64());
    let command_id = payload.get("command_id").and_then(|v| v.as_str());
    let op = payload
        .get("action")
        .and_then(|v| v.as_str())
        .map(|name| {
            json!({
                "name": name,
                "summary": payload
                    .get("command_used")
                    .or_else(|| payload.get("proposed_command"))
                    .cloned()
                    .unwrap_or_else(|| Value::String(name.to_string())),
            })
        })
        .or_else(|| {
            payload
                .get("proposed_action")
                .and_then(|v| v.as_str())
                .map(|name| {
                    json!({
                        "name": name,
                        "summary": payload
                            .get("proposed_command")
                            .cloned()
                            .unwrap_or_else(|| Value::String(name.to_string())),
                    })
                })
        });
    let ok = payload.get("success").and_then(|v| v.as_bool());
    let text = payload
        .get("summary")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("result").and_then(|v| v.as_str()))
        .or_else(|| payload.get("reason").and_then(|v| v.as_str()))
        .map(str::to_string);
    let (observation, rationale) = payload
        .get("text")
        .and_then(|v| v.as_str())
        .map(observation_and_rationale_from_text)
        .unwrap_or((None, None));
    let record = compact_log_record(
        "orch",
        event,
        actor,
        lane,
        endpoint_id,
        step,
        turn_id,
        command_id,
        op,
        ok,
        observation,
        rationale,
        text,
        filtered_payload_meta(&payload),
    );
    let _ = append_action_log_record(&record);
}

pub(crate) fn now_ms() -> u64 {
    let ms = canon_llm::endpoint_worker::tab_manager_now_ms();
    u64::try_from(ms).unwrap_or(u64::MAX)
}
