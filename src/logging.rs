use crate::llm_runtime::config::LlmEndpoint;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::canonical_writer::CanonicalWriter;
use crate::constants::MAX_SNIPPET;
use crate::prompts::{action_observation, action_rationale, parse_actions, truncate};

struct LogPaths {
    action_log: PathBuf,
}
static LOG_PATHS: OnceLock<LogPaths> = OnceLock::new();

pub fn init_log_paths(prefix: &str) {
    // Ensure canonical event log directory exists (state/event_log/event.tlog.d)
    let event_log_dir = std::path::Path::new(crate::constants::workspace())
        .join("state")
        .join("event_log")
        .join("event.tlog.d");
    let _ = std::fs::create_dir_all(&event_log_dir);
    if prefix == "supervisor" {
        return;
    }
    let base = std::path::Path::new(crate::constants::agent_state_dir()).join(prefix);
    let _ = std::fs::create_dir_all(&base);
    let _ = LOG_PATHS.set(LogPaths {
        action_log: base.join("actions.jsonl"),
    });
}

#[cfg(test)]
pub(crate) fn current_action_log_path_for_tests() -> Option<PathBuf> {
    LOG_PATHS.get().map(|paths| paths.action_log.clone())
}

#[cfg(test)]
fn log_paths() -> Result<&'static LogPaths> {
    LOG_PATHS
        .get()
        .ok_or_else(|| anyhow::anyhow!("log paths not initialized"))
}

fn patch_summary_path(patch: &str) -> Option<&str> {
    for line in patch.lines() {
        if let Some(rest) = line
            .strip_prefix("*** Update File:")
            .or_else(|| line.strip_prefix("*** Add File:"))
        {
            return Some(rest.trim());
        }
    }
    None
}

fn action_command_summary(action: &Value) -> String {
    let str_field = |key: &str| action.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let kind = action
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    match kind {
        "run_command" => str_field("cmd").to_string(),
        "python" => {
            let code = str_field("code");
            let first = code.lines().next().unwrap_or("");
            format!("python: {}", truncate(first, 160))
        }
        "read_file" => {
            let path = str_field("path");
            let line = action.get("line").and_then(|v| v.as_u64());
            match line {
                Some(n) => format!("read_file {}:{}", path, n),
                None => format!("read_file {}", path),
            }
        }
        "list_dir" => format!("list_dir {}", str_field("path")),
        "apply_patch" => {
            let patch = str_field("patch");
            patch_summary_path(patch)
                .map(|path| format!("apply_patch {}", path))
                .unwrap_or_else(|| "apply_patch".to_string())
        }
        "message" => {
            let status = str_field("status");
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

fn action_log_text(observation: &str, rationale: &str) -> Option<String> {
    match (observation.is_empty(), rationale.is_empty()) {
        (false, false) => Some(format!("{} | {}", observation, rationale)),
        (false, true) => Some(observation.to_string()),
        (true, false) => Some(rationale.to_string()),
        (true, true) => None,
    }
}

/// Intent: pure_transform
/// Resource: action_text
/// Inputs: &str
/// Outputs: std::option::Option<serde_json::Value>
/// Effects: parses first structured action from text without mutation
/// Forbidden: filesystem writes, state mutation, process spawning, network access
/// Invariants: parse failures or empty action lists return None; first parsed action is selected when present
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
fn parse_action_from_text(text: &str) -> Option<Value> {
    parse_actions(text)
        .ok()
        .and_then(|actions| actions.into_iter().next())
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

/// Intent: event_append
/// Resource: error
/// Inputs: &std::path::PathBuf, &serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn append_record_to_path(path: &PathBuf, record: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
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

/// Intent: event_append
/// Resource: error
/// Inputs: &serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(crate) fn append_action_log_record(record: &Value) -> Result<()> {
    static LOG_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = LOG_MUTEX.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().expect("action log mutex poisoned");

    let Some(paths) = LOG_PATHS.get() else {
        return Ok(());
    };
    let primary = paths.action_log.clone();
    append_record_to_path(&primary, record)?;
    Ok(())
}

pub(crate) fn make_command_id(role: &str, prompt_kind: &str, step: usize) -> String {
    format!("{}:{}:{:04}:{}", role, prompt_kind, step, now_ms())
}

fn compact_json(value: Value) -> Option<Value> {
    enum Frame {
        Visit(Value),
        FinishArray(usize),
        FinishObject(Vec<String>),
    }

    let mut frames = vec![Frame::Visit(value)];
    let mut results: Vec<Option<Value>> = Vec::new();

    while let Some(frame) = frames.pop() {
        match frame {
            Frame::Visit(Value::Null) => results.push(None),
            Frame::Visit(Value::String(text)) => results.push(compact_json_string(text)),
            Frame::Visit(Value::Array(items)) => {
                let len = items.len();
                frames.push(Frame::FinishArray(len));
                for item in items.into_iter().rev() {
                    frames.push(Frame::Visit(item));
                }
            }
            Frame::Visit(Value::Object(fields)) => {
                let entries = fields.into_iter().collect::<Vec<_>>();
                let keys = entries
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                frames.push(Frame::FinishObject(keys));
                for (_, value) in entries.into_iter().rev() {
                    frames.push(Frame::Visit(value));
                }
            }
            Frame::Visit(other) => results.push(Some(other)),
            Frame::FinishArray(len) => {
                let compacted = compact_json_finish_array(&mut results, len);
                results.push(compacted);
            }
            Frame::FinishObject(keys) => {
                let compacted = compact_json_finish_object(&mut results, keys);
                results.push(compacted);
            }
        }
    }

    results.pop().flatten()
}

fn compact_json_string(text: String) -> Option<Value> {
    let text = text.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(Value::String(text))
    }
}

fn compact_json_finish_array(results: &mut Vec<Option<Value>>, len: usize) -> Option<Value> {
    let mut items = Vec::with_capacity(len);
    for _ in 0..len {
        items.push(results.pop().expect("array child result"));
    }
    items.reverse();
    let items = items.into_iter().flatten().collect::<Vec<_>>();
    (!items.is_empty()).then_some(Value::Array(items))
}

fn compact_json_finish_object(
    results: &mut Vec<Option<Value>>,
    keys: Vec<String>,
) -> Option<Value> {
    let mut values = Vec::with_capacity(keys.len());
    for _ in 0..keys.len() {
        values.push(results.pop().expect("object child result"));
    }
    values.reverse();

    let mut out = serde_json::Map::new();
    for (key, value) in keys.into_iter().zip(values.into_iter()) {
        if let Some(value) = value {
            out.insert(key, value);
        }
    }

    (!out.is_empty()).then_some(Value::Object(out))
}

fn insert_compact_log_field(
    record: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<Value>,
) {
    if let Some(v) = value.and_then(compact_json) {
        record.insert(key.to_string(), v);
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
        json!(crate::llm_runtime::tab_management::tab_manager_now_ms()),
    );
    record.insert("kind".to_string(), json!(kind));
    record.insert("phase".to_string(), json!(phase));

    insert_compact_log_context_fields(
        &mut record,
        actor,
        lane,
        endpoint_id,
        step,
        turn_id,
        command_id,
        op,
        ok,
    );
    insert_compact_log_text_fields(&mut record, observation, rationale, text, meta);

    Value::Object(record)
}

fn insert_compact_log_context_fields(
    record: &mut serde_json::Map<String, Value>,
    actor: Option<&str>,
    lane: Option<&str>,
    endpoint_id: Option<&str>,
    step: Option<usize>,
    turn_id: Option<u64>,
    command_id: Option<&str>,
    op: Option<Value>,
    ok: Option<bool>,
) {
    insert_compact_log_field(record, "actor", actor.map(|v| json!(v)));
    insert_compact_log_field(record, "lane", lane.map(|v| json!(v)));
    insert_compact_log_field(record, "endpoint_id", endpoint_id.map(|v| json!(v)));
    insert_compact_log_field(record, "step", step.map(|v| json!(v)));
    insert_compact_log_field(record, "turn_id", turn_id.map(|v| json!(v)));
    insert_compact_log_field(record, "command_id", command_id.map(|v| json!(v)));
    insert_compact_log_field(record, "op", op);
    insert_compact_log_field(record, "ok", ok.map(|v| json!(v)));
}

fn insert_compact_log_text_fields(
    record: &mut serde_json::Map<String, Value>,
    observation: Option<String>,
    rationale: Option<String>,
    text: Option<String>,
    meta: Option<Value>,
) {
    insert_compact_log_field(
        record,
        "observation",
        observation.map(|v| json!(truncate(&v, MAX_SNIPPET))),
    );
    insert_compact_log_field(
        record,
        "rationale",
        rationale.map(|v| json!(truncate(&v, MAX_SNIPPET))),
    );
    insert_compact_log_field(
        record,
        "text",
        text.map(|v| json!(truncate(&v, MAX_SNIPPET))),
    );
    insert_compact_log_field(record, "meta", meta);
}

fn action_op(action: &Value) -> Option<Value> {
    let name = action.get("action").and_then(|v| v.as_str())?;
    Some(build_action_op(name, action_command_summary(action)))
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, std::string::String
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_action_op(name: &str, summary: String) -> Value {
    json!({
        "name": name,
        "summary": summary,
    })
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
    insert_if_missing("task_id", action.get("task_id").cloned());
    insert_if_missing("objective_id", action.get("objective_id").cloned());
    insert_if_missing("intent", action.get("intent").cloned());
    // Ensure message actions preserve routing + payload metadata
    if action.get("action").and_then(|v| v.as_str()) == Some("message") {
        insert_if_missing("from", action.get("from").cloned());
        insert_if_missing("to", action.get("to").cloned());
        insert_if_missing("type", action.get("type").cloned());
        insert_if_missing("status", action.get("status").cloned());
        insert_if_missing("payload", action.get("payload").cloned());
    }
}

/// Intent: event_append
/// Resource: error
/// Inputs: &str, &llm_runtime::config::LlmEndpoint, &str, usize, &str, &str, serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
        log_append_failure(role, step, "action_log_error", &err);
    }
}

/// Intent: event_append
/// Resource: error
/// Inputs: &str, &llm_runtime::config::LlmEndpoint, &str, usize, &str, &serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(crate) fn append_action_log(
    role: &str,
    endpoint: &LlmEndpoint,
    _prompt_kind: &str,
    step: usize,
    command_id: &str,
    action: &Value,
) -> Result<()> {
    let observation = action_observation(action).unwrap_or("");
    let rationale = action_rationale(action).unwrap_or("");
    let text = action_log_text(observation, rationale);
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
    append_action_log_record(&record)
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
        log_append_failure(role, step, "action_log_error", &e);
    }
}

/// Intent: event_append
/// Resource: action_result_log
/// Inputs: std::option::Option<&mut canonical_writer::CanonicalWriter>, &str, &llm_runtime::config::LlmEndpoint, &str, usize, &str, &serde_json::Value, bool, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: appends action result log record and records canonical effect when writer is present
/// Forbidden: network access, process spawning
/// Invariants: log record and effect include role, step, command id, action kind, success flag, result bytes, and result hash
/// Failure: returns action log append errors
/// Provenance: rustc:facts + rustc:docstring
pub(crate) fn append_action_result_log(
    mut writer: Option<&mut CanonicalWriter>,
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
    append_action_log_record(&record)?;

    let effect = crate::events::EffectEvent::ActionResultRecorded {
        role: role.to_string(),
        step,
        command_id: command_id.to_string(),
        action_kind: action
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        task_id: action
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        objective_id: action
            .get("objective_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ok: success,
        result_bytes: result_text.len(),
        result_hash: stable_hash_hex(result_text),
        result: result_text.to_string(),
    };
    if let Some(writer) = writer.as_deref_mut() {
        writer.record_effect(effect);
    }

    Ok(())
}

pub(crate) fn log_action_result(
    writer: Option<&mut CanonicalWriter>,
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
        writer,
        role,
        endpoint,
        prompt_kind,
        step,
        command_id,
        action,
        success,
        output,
    ) {
        log_append_failure(role, step, "action_result_log_error", &e);
    }
}

pub fn log_error_event(
    role: &str,
    phase: &str,
    step: Option<usize>,
    text: &str,
    meta: Option<Value>,
) {
    let record = compact_log_record(
        "error",
        phase,
        Some(role),
        None,
        None,
        step,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(text.to_string()),
        meta,
    );
    if let Err(err) = append_action_log_record(&record) {
        log_append_failure(role, step.unwrap_or(0), "error_log_error", &err);
    }
}

fn log_append_failure(role: &str, step: usize, label: &str, err: &dyn std::fmt::Display) {
    eprintln!("[{role}] step={step} {label}: {err}");
}

/// Intent: event_append
/// Resource: llm_completion_log
/// Inputs: &str, &llm_runtime::config::LlmEndpoint, usize, &str, &serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: appends compact LLM completion action log record
/// Forbidden: network access, process spawning, mutation outside action log append path
/// Invariants: record includes role, endpoint id, step, command id, action op, observation, rationale, and injected action fields
/// Failure: returns action log append errors
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: event_append
/// Resource: error
/// Inputs: &str, serde_json::Value
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(crate) fn append_orchestration_trace(event: &str, payload: Value) {
    if LOG_PATHS.get().is_none() {
        return;
    }
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
    let proposed_command = payload.get("proposed_command").cloned();
    let op_summary = |name: &str, summary: Option<Value>| {
        json!({
            "name": name,
            "summary": summary.unwrap_or_else(|| Value::String(name.to_string())),
        })
    };
    let command_summary = payload
        .get("command_used")
        .cloned()
        .or_else(|| proposed_command.clone());
    let op = if let Some(name) = payload.get("action").and_then(|v| v.as_str()) {
        Some(op_summary(name, command_summary))
    } else {
        payload
            .get("proposed_action")
            .and_then(|v| v.as_str())
            .map(|name| op_summary(name, proposed_command.clone()))
    };
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
    if let Err(err) = append_action_log_record(&record) {
        eprintln!("[trace] orchestration_log_error: {err}");
    }
}

/// Append a violation to VIOLATIONS.json when a prompt exceeds the overflow threshold.
///
/// Only fires when `prompt_bytes > PROMPT_OVERFLOW_BYTES`.  Deduplicates by checking
/// whether an open violation with the same role already exists — avoids flooding the
/// violations file with one entry per cycle.
fn evidence_receipts_path() -> PathBuf {
    std::path::Path::new(crate::constants::agent_state_dir()).join("evidence_receipts.jsonl")
}

pub(crate) fn stable_hash_hex(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Intent: event_append
/// Resource: error
/// Inputs: &str, &str, std::option::Option<&str>, std::option::Option<std::path::PathBuf>, serde_json::Value, &str
/// Outputs: std::result::Result<std::string::String, anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn append_evidence_receipt(
    role: &str,
    action: &str,
    rel_path: Option<&str>,
    abs_path: Option<PathBuf>,
    meta: Value,
    output: &str,
) -> Result<String> {
    #[derive(serde::Serialize)]
    struct EvidenceReceipt {
        id: String,
        ts_ms: u64,
        actor: String,
        step: usize,
        action: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        abs_path: Option<String>,
        meta: Value,
        output_hash: String,
    }

    let ts_ms = now_ms();
    let id = format!("rcpt-{ts_ms}-{role}-0-{action}");
    let receipt = EvidenceReceipt {
        id: id.clone(),
        ts_ms,
        actor: role.to_string(),
        step: 0,
        action: action.to_string(),
        path: rel_path.map(str::to_string),
        abs_path: abs_path.map(|p| p.display().to_string()),
        meta,
        output_hash: stable_hash_hex(output),
    };
    let path = evidence_receipts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", serde_json::to_string(&receipt)?)?;
    Ok(id)
}

pub fn record_workspace_artifact_effect(
    workspace: &std::path::Path,
    requested: bool,
    artifact: &str,
    op: &str,
    target: &str,
    subject: &str,
    signature: &str,
) -> Result<()> {
    let artifact_id = workspace_artifact_id(artifact, target, subject, signature);
    record_workspace_artifact_effect_with_id(
        workspace,
        requested,
        &artifact_id,
        artifact,
        op,
        target,
        subject,
        signature,
    )
}

pub fn record_effect_for_workspace(
    workspace: &std::path::Path,
    effect: crate::events::EffectEvent,
) -> Result<()> {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let state = crate::system_state::SystemState::new(&[], 0);
    let mut writer = crate::canonical_writer::CanonicalWriter::try_new(
        state,
        crate::tlog::Tlog::open(&tlog_path),
        workspace.to_path_buf(),
    )?;
    writer
        .try_record_effect(effect)
        .map_err(|err| anyhow::anyhow!("canonical effect append failed: {err:#}"))
}

pub fn artifact_write_signature(parts: &[&str]) -> String {
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn workspace_artifact_id(
    kind: &str,
    target: &str,
    subject: &str,
    content_hash: &str,
) -> String {
    let mut hasher = DefaultHasher::new();
    (kind, target, subject, content_hash).hash(&mut hasher);
    format!("artifact-{:016x}", hasher.finish())
}

fn record_workspace_artifact_effect_with_id(
    workspace: &std::path::Path,
    requested: bool,
    artifact_id: &str,
    artifact: &str,
    op: &str,
    target: &str,
    subject: &str,
    signature: &str,
) -> Result<()> {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let state = crate::system_state::SystemState::new(&[], 0);
    let mut writer = crate::canonical_writer::CanonicalWriter::try_new(
        state,
        crate::tlog::Tlog::open(&tlog_path),
        workspace.to_path_buf(),
    )?;
    let effect = if requested {
        crate::events::EffectEvent::WorkspaceArtifactWriteRequested {
            artifact_id: artifact_id.to_string(),
            artifact: artifact.to_string(),
            op: op.to_string(),
            target: target.to_string(),
            subject: subject.to_string(),
            signature: signature.to_string(),
        }
    } else {
        crate::events::EffectEvent::WorkspaceArtifactWriteApplied {
            artifact_id: artifact_id.to_string(),
            artifact: artifact.to_string(),
            op: op.to_string(),
            target: target.to_string(),
            subject: subject.to_string(),
            signature: signature.to_string(),
        }
    };
    writer.try_record_effect(effect).map_err(|err| {
        anyhow::anyhow!(
            "canonical effect append failed for {} {} {}: {err:#}",
            artifact,
            op,
            subject
        )
    })
}

fn file_snapshot(path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn restore_file_snapshot(path: &std::path::Path, snapshot: &Option<Vec<u8>>) -> Result<()> {
    match snapshot {
        Some(bytes) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            std::fs::write(path, bytes).with_context(|| format!("restore {}", path.display()))?;
        }
        None => {
            if path.exists() {
                std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &std::path::Path, &str, &str, &str, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: fs_read, fs_write, logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn write_projection_with_artifact_effects(
    workspace: &std::path::Path,
    path: &std::path::Path,
    artifact: &str,
    op: &str,
    subject: &str,
    contents: &str,
) -> Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if stable_hash_hex(&existing) == stable_hash_hex(contents) {
            return Ok(());
        }
    }
    let content_hash = stable_hash_hex(contents);
    let signature = artifact_write_signature(&[artifact, op, subject, &content_hash]);
    let target = path.to_string_lossy().into_owned();
    let artifact_id = workspace_artifact_id(artifact, &target, subject, &content_hash);
    record_workspace_artifact_effect_with_id(
        workspace,
        true,
        &artifact_id,
        artifact,
        op,
        &target,
        subject,
        &signature,
    )?;
    let snapshot = file_snapshot(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, contents).with_context(|| format!("write {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;
    if let Err(err) = record_workspace_artifact_effect_with_id(
        workspace,
        false,
        &artifact_id,
        artifact,
        op,
        &target,
        subject,
        &signature,
    ) {
        restore_file_snapshot(path, &snapshot)?;
        return Err(err);
    }
    Ok(())
}

pub fn record_json_projection_with_optional_writer<T: Serialize>(
    workspace: &std::path::Path,
    path: &std::path::Path,
    artifact: &str,
    op: &str,
    subject: &str,
    payload: &T,
    writer: Option<&mut CanonicalWriter>,
    effect: Option<crate::events::EffectEvent>,
) -> Result<()> {
    let serialized_payload = serde_json::to_string_pretty(payload)?;
    record_serialized_json_projection_with_optional_writer(
        workspace,
        path,
        artifact,
        op,
        subject,
        &serialized_payload,
        writer,
        effect,
    )
}

pub fn record_serialized_json_projection_with_optional_writer(
    workspace: &std::path::Path,
    path: &std::path::Path,
    artifact: &str,
    op: &str,
    subject: &str,
    serialized_payload: &str,
    writer: Option<&mut CanonicalWriter>,
    effect: Option<crate::events::EffectEvent>,
) -> Result<()> {
    if let Some(effect) = effect {
        if let Some(writer_ref) = writer {
            writer_ref.try_record_effect(effect)?;
        } else {
            record_effect_for_workspace(workspace, effect)?;
        }
    }
    write_projection_with_artifact_effects(
        workspace,
        path,
        artifact,
        op,
        subject,
        serialized_payload,
    )
}

/// Intent: repair_or_initialize
/// Resource: error
/// Inputs: &std::path::Path, &str, &str, &str, &str
/// Outputs: std::result::Result<bool, anyhow::Error>
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn migrate_projection_if_present(
    workspace: &std::path::Path,
    legacy_rel: &str,
    canonical_rel: &str,
    artifact: &str,
    subject: &str,
) -> Result<bool> {
    let legacy_path = workspace.join(legacy_rel);
    if !legacy_path.is_file() {
        return Ok(false);
    }

    let canonical_path = workspace.join(canonical_rel);
    if canonical_path.exists() {
        return Ok(false);
    }

    let contents = std::fs::read_to_string(&legacy_path)
        .with_context(|| format!("read {}", legacy_path.display()))?;
    write_projection_with_artifact_effects(
        workspace,
        &canonical_path,
        artifact,
        "migrate",
        subject,
        &contents,
    )?;
    std::fs::remove_file(&legacy_path)
        .with_context(|| format!("remove {}", legacy_path.display()))?;
    Ok(true)
}

/// Intent: diagnostic_scan
/// Resource: prompt_overflow_report_json
/// Inputs: &str
/// Outputs: serde_json::Map<std::string::String, serde_json::Value>
/// Effects: none
/// Forbidden: mutation
/// Invariants: returns an object map only; empty, invalid, or non-object JSON returns an empty map
/// Failure: malformed JSON is ignored and treated as an empty report
/// Provenance: rustc:facts + rustc:docstring
fn parse_prompt_overflow_report(raw: &str) -> serde_json::Map<String, Value> {
    if raw.trim().is_empty() {
        serde_json::Map::new()
    } else {
        serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }
}

fn prompt_overflow_violation_id(role: &str) -> String {
    format!(
        "PROMPT-OVERFLOW-{}",
        role.to_uppercase().replace(['[', ']', '/'], "-")
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &std::path::Path, &str, &serde_json::Map<std::string::String, serde_json::Value>
/// Outputs: ()
/// Effects: fs_write, state_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn persist_prompt_overflow_report(
    workspace: &std::path::Path,
    violations_path: &std::path::Path,
    role: &str,
    report: &serde_json::Map<String, Value>,
) {
    if let Ok(body) = serde_json::to_string_pretty(&Value::Object(report.clone())) {
        let _ = write_projection_with_artifact_effects(
            workspace,
            violations_path,
            crate::constants::VIOLATIONS_FILE,
            "append",
            &format!("prompt_overflow:{role}"),
            &body,
        );
    }
}

fn mark_prompt_overflow_failed(report: &mut serde_json::Map<String, Value>) {
    report.insert("status".to_string(), json!("failed"));
    report.entry("summary".to_string()).or_insert(json!(
        "One or more agent roles are sending prompts that exceed the noise-context threshold."
    ));
}

/// Intent: event_append
/// Resource: error
/// Inputs: &str, usize, usize, &std::path::Path, bool
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn append_prompt_overflow_receipt(
    role: &str,
    prompt_bytes: usize,
    threshold: usize,
    violations_path: &std::path::Path,
    reconciled_duplicate: bool,
) -> Option<String> {
    append_evidence_receipt(
        role,
        "prompt_overflow",
        Some(crate::constants::VIOLATIONS_FILE),
        Some(violations_path.to_path_buf()),
        json!({
            "kind": "prompt_overflow",
            "role": role,
            "prompt_bytes": prompt_bytes,
            "threshold": threshold,
            "reconciled_duplicate": reconciled_duplicate,
        }),
        &format!("role={role};prompt_bytes={prompt_bytes};threshold={threshold}"),
    )
    .ok()
}

fn refresh_existing_prompt_overflow_violation(
    workspace: &std::path::Path,
    violations_path: &std::path::Path,
    role: &str,
    prompt_bytes: usize,
    report: &mut serde_json::Map<String, Value>,
    violations: Vec<Value>,
    existing_index: usize,
) {
    use crate::constants::PROMPT_OVERFLOW_BYTES;

    let receipts_raw = std::fs::read_to_string(evidence_receipts_path()).unwrap_or_default();
    let existing_receipt_id = violations[existing_index]
        .get("evidence_receipts")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let receipt_missing = existing_receipt_id
        .as_deref()
        .map_or(true, |receipt_id| !receipts_raw.contains(receipt_id));

    if !receipt_missing {
        return;
    }

    let refreshed_receipt_id = append_prompt_overflow_receipt(
        role,
        prompt_bytes,
        PROMPT_OVERFLOW_BYTES,
        violations_path,
        true,
    );

    let mut reconciled_violations = violations;
    if let Some(existing) = reconciled_violations.get_mut(existing_index) {
        if let Some(existing_obj) = existing.as_object_mut() {
            existing_obj.insert(
                "evidence_receipts".to_string(),
                Value::Array(
                    refreshed_receipt_id
                        .clone()
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                ),
            );
            existing_obj.insert(
                "evidence_hashes".to_string(),
                json!([format!("prompt_bytes:{}:{}", role, prompt_bytes)]),
            );
            existing_obj.insert("last_validated_ms".to_string(), json!(now_ms()));
            existing_obj.insert("freshness_status".to_string(), json!("fresh"));
            existing_obj.insert(
                "validated_from".to_string(),
                json!(["runtime/prompt_bytes"]),
            );
        }
    }
    report.insert(
        "violations".to_string(),
        Value::Array(reconciled_violations),
    );
    mark_prompt_overflow_failed(report);
    persist_prompt_overflow_report(workspace, violations_path, role, report);
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path, &str, usize
/// Outputs: ()
/// Effects: fs_read, state_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(crate) fn record_prompt_overflow(workspace: &std::path::Path, role: &str, prompt_bytes: usize) {
    use crate::constants::PROMPT_OVERFLOW_BYTES;
    if prompt_bytes <= PROMPT_OVERFLOW_BYTES {
        return;
    }
    let violations_path = workspace.join(crate::constants::VIOLATIONS_FILE);
    let raw = std::fs::read_to_string(&violations_path).unwrap_or_default();

    let mut report = parse_prompt_overflow_report(&raw);

    // Deduplicate: skip if an open overflow violation for this role already exists.
    let violation_id = prompt_overflow_violation_id(role);
    let violations = report
        .get("violations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if let Some(existing_index) = violations
        .iter()
        .position(|v| v.get("id").and_then(|x| x.as_str()) == Some(&violation_id))
    {
        refresh_existing_prompt_overflow_violation(
            workspace,
            &violations_path,
            role,
            prompt_bytes,
            &mut report,
            violations,
            existing_index,
        );
        return;
    }

    let receipt_id = append_prompt_overflow_receipt(
        role,
        prompt_bytes,
        PROMPT_OVERFLOW_BYTES,
        &violations_path,
        false,
    );
    let new_violation = json!({
        "id": violation_id,
        "title": format!("Prompt overflow: {} sent {prompt_bytes} bytes (limit {PROMPT_OVERFLOW_BYTES})", role),
        "severity": "high",
        "evidence": [
            format!("role={role} prompt_bytes={prompt_bytes} threshold={PROMPT_OVERFLOW_BYTES}"),
            "Large prompts flood the model with noise context and degrade focus on the highest-priority work.",
            "Check which context sections are over-sized: ISSUES.json (use top-N), OBJECTIVES.json (use compact), complexity report (use hotspots only)."
        ],
        "issue": format!("Prompt for role `{role}` exceeds the {PROMPT_OVERFLOW_BYTES}-byte noise threshold at {prompt_bytes} bytes."),
        "impact": "Model focus degrades; low-signal context crowds out high-priority signals.",
        "required_fix": [
            format!("Identify which injected section accounts for the excess (prompt_bytes={prompt_bytes})."),
            "Trim the over-sized section to a top-N summary or compact format.",
            "Verify the prompt drops below the threshold after trimming."
        ],
        "freshness_status": "fresh",
        "validated_from": ["runtime/prompt_bytes"],
        "evidence_receipts": receipt_id.into_iter().collect::<Vec<_>>(),
        "evidence_hashes": [format!("prompt_bytes:{}:{}", role, prompt_bytes)],
        "last_validated_ms": now_ms()
    });

    let mut new_violations = violations;
    new_violations.push(new_violation);
    report.insert("violations".to_string(), Value::Array(new_violations));
    mark_prompt_overflow_failed(&mut report);

    persist_prompt_overflow_report(workspace, &violations_path, role, &report);
    eprintln!(
        "[{role}] PROMPT OVERFLOW: {prompt_bytes} bytes exceeds threshold {PROMPT_OVERFLOW_BYTES}"
    );
}

pub(crate) fn now_ms() -> u64 {
    let ms = crate::llm_runtime::tab_management::tab_manager_now_ms();
    u64::try_from(ms).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        append_orchestration_trace, log_error_event, log_paths, now_ms, record_prompt_overflow,
        stable_hash_hex, workspace_artifact_id, write_projection_with_artifact_effects, LogPaths,
        LOG_PATHS,
    };
    use serde_json::{json, Value};
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    fn global_state_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn temp_workspace(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "canon-mini-agent-logging-test-{}-{}-{}",
            name,
            std::process::id(),
            now_ms()
        ));
        if root.exists() {
            let _ = fs::remove_dir_all(&root);
        }
        fs::create_dir_all(&root).expect("create temp workspace");
        root
    }

    fn read_record_with_text(action_log: &std::path::Path, expected_text: &str) -> Value {
        let log_text = fs::read_to_string(action_log).expect("read action log");
        for line in log_text.lines().rev() {
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if record.get("text").and_then(|v| v.as_str()) == Some(expected_text) {
                return record;
            }
        }
        panic!("expected record with matching text not found in action log");
    }

    fn assert_common_error_shape(record: &Value, actor: &str, phase: &str, step: Option<u64>) {
        assert_eq!(record.get("kind").and_then(|v| v.as_str()), Some("error"));
        assert_eq!(record.get("phase").and_then(|v| v.as_str()), Some(phase));
        assert_eq!(record.get("actor").and_then(|v| v.as_str()), Some(actor));
        assert_eq!(record.get("step").and_then(|v| v.as_u64()), step);
        assert!(record.get("ts_ms").and_then(|v| v.as_u64()).is_some());
        assert!(record.get("text").and_then(|v| v.as_str()).is_some());
        assert!(record.get("meta").is_some(), "meta field should be present");
        assert!(
            record.get("meta").is_some_and(|v| v.is_object()),
            "meta must be a JSON object"
        );
    }

    #[test]
    fn workspace_artifact_write_events_include_stable_artifact_id() {
        let _guard = global_state_lock().lock().expect("lock global state");
        let workspace = temp_workspace("artifact-id");
        let path = workspace.join("agent_state").join("artifact_id_test.json");
        let contents = "{\"ok\":true}\n";
        let target = path.to_string_lossy().into_owned();
        let content_hash = stable_hash_hex(contents);
        let expected_id = workspace_artifact_id(
            "agent_state/artifact_id_test.json",
            &target,
            "artifact_id_test",
            &content_hash,
        );

        write_projection_with_artifact_effects(
            &workspace,
            &path,
            "agent_state/artifact_id_test.json",
            "write",
            "artifact_id_test",
            contents,
        )
        .expect("write projection");

        let tlog_raw = fs::read_to_string(workspace.join("agent_state").join("tlog.ndjson"))
            .expect("read tlog");
        let mut artifact_ids = Vec::new();
        for line in tlog_raw.lines() {
            let record: Value = serde_json::from_str(line).expect("parse tlog record");
            let Some(event) = record.get("event").and_then(|v| v.get("event")) else {
                continue;
            };
            let kind = event.get("kind").and_then(|v| v.as_str());
            if matches!(
                kind,
                Some("workspace_artifact_write_requested")
                    | Some("workspace_artifact_write_applied")
            ) {
                artifact_ids.push(
                    event
                        .get("artifact_id")
                        .and_then(|v| v.as_str())
                        .expect("artifact_id")
                        .to_string(),
                );
            }
        }

        assert_eq!(artifact_ids, vec![expected_id.clone(), expected_id]);
    }

    fn ensure_test_action_log_path() -> std::path::PathBuf {
        if let Some(paths) = LOG_PATHS.get() {
            return paths.action_log.clone();
        }

        let root = std::env::temp_dir().join(format!("canon-mini-agent-logging-test-{}", now_ms()));
        let action_log = root.join("actions.jsonl");
        let _ = LOG_PATHS.set(LogPaths {
            action_log: action_log.clone(),
        });
        log_paths()
            .expect("test log paths should initialize")
            .action_log
            .clone()
    }

    #[test]
    fn log_error_event_persists_structured_error_record() {
        let action_log = ensure_test_action_log_path();
        if let Some(parent) = action_log.parent() {
            fs::create_dir_all(parent).expect("create action log parent");
        }

        let marker = format!("logging persistence marker {}", now_ms());
        log_error_event(
            "solo",
            "test_phase",
            Some(7),
            &marker,
            Some(json!({ "case": "persisted_error_event" })),
        );

        let record = read_record_with_text(&action_log, &marker);
        assert_common_error_shape(&record, "solo", "test_phase", Some(7));
        assert_eq!(
            record.get("text").and_then(|v| v.as_str()),
            Some(marker.as_str())
        );
        assert_eq!(
            record
                .get("meta")
                .and_then(|v| v.get("case"))
                .and_then(|v| v.as_str()),
            Some("persisted_error_event")
        );
    }

    #[test]
    fn log_error_event_preserves_consistent_shape_across_error_categories() {
        let action_log = ensure_test_action_log_path();
        if let Some(parent) = action_log.parent() {
            fs::create_dir_all(parent).expect("create action log parent");
        }

        let cases = vec![
            (
                "executor",
                "orchestrate",
                None,
                format!("executor timeout {}", now_ms()),
                json!({
                    "stage": "executor_submit_timeout",
                    "lane": "lane-a",
                    "command_id": "executor:executor:0001:123"
                }),
            ),
            (
                "solo",
                "run_command",
                Some(3),
                format!("run command failed {}", now_ms()),
                json!({
                    "cmd": "cargo test -p canon-mini-agent",
                    "cwd": "/workspace/ai_sandbox/canon-mini-agent"
                }),
            ),
            (
                "supervisor",
                "supervisor_main",
                None,
                format!("binary updated {}", now_ms()),
                json!({
                    "stage": "restart_pending",
                    "path": "target/debug/canon-mini-agent"
                }),
            ),
        ];

        for (actor, phase, step, text, meta) in cases {
            log_error_event(actor, phase, step, &text, Some(meta.clone()));
            let record = read_record_with_text(&action_log, &text);
            assert_common_error_shape(&record, actor, phase, step.map(|v| v as u64));
            assert_eq!(
                record.get("text").and_then(|v| v.as_str()),
                Some(text.as_str())
            );
            assert_eq!(record.get("meta"), Some(&meta));
        }
    }

    #[test]
    fn append_orchestration_trace_before_init_is_silent_and_does_not_create_logs() {
        let unique = now_ms();
        let probe_root =
            std::env::temp_dir().join(format!("canon-mini-agent-preinit-trace-probe-{unique}"));
        let action_log = probe_root.join("actions.jsonl");

        append_orchestration_trace(
            "pre_init_probe",
            json!({
                "role": "solo",
                "summary": format!("pre-init probe {unique}")
            }),
        );

        assert!(
            !action_log.exists(),
            "pre-init orchestration trace should not create an action log before init_log_paths"
        );
    }

    #[test]
    fn record_prompt_overflow_appends_real_receipt_and_canonical_effects() {
        let _guard = global_state_lock().lock().expect("lock");
        let workspace = temp_workspace("prompt-overflow-canonical-effects");
        let state_dir = workspace.join("agent_state");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&state_dir).expect("create state dir");
        crate::constants::set_workspace(workspace.to_string_lossy().to_string());
        crate::constants::set_agent_state_dir(state_dir.to_string_lossy().to_string());

        record_prompt_overflow(
            &workspace,
            "diagnostics",
            crate::constants::PROMPT_OVERFLOW_BYTES + 1,
        );

        let violations_path = workspace.join(crate::constants::VIOLATIONS_FILE);
        let violations: Value =
            serde_json::from_str(&fs::read_to_string(&violations_path).expect("read violations"))
                .expect("parse violations");
        let receipt_id = violations
            .get("violations")
            .and_then(|v| v.as_array())
            .and_then(|items| items.last())
            .and_then(|v| v.get("evidence_receipts"))
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|v| v.as_str())
            .expect("real receipt id");
        assert!(receipt_id.starts_with("rcpt-"));
        assert!(!receipt_id.starts_with("runtime-prompt-overflow-"));

        let receipts_raw = fs::read_to_string(state_dir.join("evidence_receipts.jsonl"))
            .expect("read evidence receipts");
        assert!(receipts_raw.contains(receipt_id));

        let tlog_raw = fs::read_to_string(workspace.join("agent_state").join("tlog.ndjson"))
            .expect("read tlog");
        assert!(tlog_raw.contains("workspace_artifact_write_requested"));
        assert!(tlog_raw.contains("workspace_artifact_write_applied"));
        assert!(tlog_raw.contains(crate::constants::VIOLATIONS_FILE));
    }
}
