use serde_json::{json, Value};

use crate::prompts::{validate_message_action, MessageValidationMode};
use crate::tool_schema::{action_schema_json, schema_diff_messages};

pub fn default_message_route(
    role: &str,
) -> (&'static str, &'static str, &'static str, &'static str) {
    if role.starts_with("executor") {
        ("executor", "verifier", "handoff", "complete")
    } else if role == "verifier" {
        ("verifier", "planner", "verification", "verified")
    } else if role == "diagnostics" {
        ("diagnostics", "planner", "diagnostics", "complete")
    } else if role == "solo" {
        ("solo", "solo", "result", "complete")
    } else if role == "planner" || role == "mini_planner" {
        ("planner", "executor", "plan", "complete")
    } else {
        ("executor", "verifier", "handoff", "complete")
    }
}

struct MessageRoute<'a> {
    from: &'a str,
    to: &'a str,
    msg_type: &'a str,
    status: &'a str,
}

impl<'a> MessageRoute<'a> {
    fn default_for_role(role: &'a str) -> Self {
        let (from, to, msg_type, status) = default_message_route(role);
        Self {
            from,
            to,
            msg_type,
            status,
        }
    }

    fn from_action_obj(obj: &'a serde_json::Map<String, Value>) -> Option<Self> {
        let from = obj.get("from").and_then(|v| v.as_str())?;
        let to = obj.get("to").and_then(|v| v.as_str())?;
        let msg_type = obj.get("type").and_then(|v| v.as_str())?;
        let status = obj.get("status").and_then(|v| v.as_str())?;
        Some(Self {
            from,
            to,
            msg_type,
            status,
        })
    }

    fn into_owned_lowercase(self) -> (String, String, String, String) {
        (
            self.from.to_lowercase(),
            self.to.to_lowercase(),
            self.msg_type.to_lowercase(),
            self.status.to_lowercase(),
        )
    }
}

pub fn expected_message_format(
    from: &str,
    to: &str,
    msg_type: &str,
    status: &str,
) -> String {
    format!(
        "{{ \"action\": \"message\", \"from\": \"{from}\", \"to\": \"{to}\", \"type\": \"{msg_type}\", \"status\": \"{status}\", \"payload\": {{ \"summary\": \"...\" }} }}"
    )
}

pub fn format_message_schema(
    from: &str,
    to: &str,
    msg_type: &str,
    status: &str,
    extra_payload: &[(&str, &str)],
) -> String {
    let mut payload = vec!["\"summary\": \"...\"".to_string()];
    for (key, value) in extra_payload {
        payload.push(format!("\"{key}\": \"{value}\""));
    }
    let payload_lines = payload.join(",\n    ");
    format!(
        "```json\n{{\n  \"action\": \"message\",\n  \"from\": \"{from}\",\n  \"to\": \"{to}\",\n  \"type\": \"{msg_type}\",\n  \"status\": \"{status}\",\n  \"observation\": \"...\",\n  \"rationale\": \"...\",\n  \"payload\": {{\n    {payload_lines}\n  }}\n}}\n```"
    )
}

#[allow(dead_code)]
const GRAPH_TOOL_TEMPLATES: [&str; 4] = [
    "```json\n{\n  \"action\": \"graph_call\",\n  \"crate\": \"canon-runtime\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate call graph.\",\n  \"rationale\": \"Inspect call graph output.\"\n}\n```",
    "```json\n{\n  \"action\": \"graph_cfg\",\n  \"crate\": \"canon-runtime\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate CFG graph.\",\n  \"rationale\": \"Inspect CFG output.\"\n}\n```",
    "```json\n{\n  \"action\": \"graph_dataflow\",\n  \"crate\": \"canon-runtime\",\n  \"tlog\": \"\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate dataflow report.\",\n  \"rationale\": \"Inspect dataflow metrics.\"\n}\n```",
    "```json\n{\n  \"action\": \"graph_reachability\",\n  \"crate\": \"canon-runtime\",\n  \"tlog\": \"\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate reachability report.\",\n  \"rationale\": \"Inspect reachability metrics.\"\n}\n```",
];

fn blocker_message_example_template() -> &'static str {
    "```json\n{\n  \"action\": \"message\",\n  \"from\": \"executor\",\n  \"to\": \"planner\",\n  \"type\": \"blocker\",\n  \"status\": \"blocked\",\n  \"observation\": \"Describe the blocked state.\",\n  \"rationale\": \"Explain why progress is impossible without external action.\",\n  \"payload\": {\n    \"summary\": \"Short blocker summary\",\n    \"blocker\": \"Root cause\",\n    \"evidence\": \"Concrete error text or failing command\",\n    \"required_action\": \"What must be done to unblock\",\n    \"severity\": \"error\"\n  }\n}\n```"
}

#[allow(dead_code)]
pub(crate) fn unsupported_action_templates() -> Vec<String> {
    let mut templates = vec![
        "```json\n{\n  \"action\": \"list_dir\",\n  \"path\": \".\",\n  \"observation\": \"List workspace root to locate targets.\",\n  \"rationale\": \"Confirm files before acting.\"\n}\n```",
        "```json\n{\n  \"action\": \"read_file\",\n  \"path\": \"canon-utils/canon-loop/src/executor.rs\",\n  \"observation\": \"Read file to understand current logic.\",\n  \"rationale\": \"Need context before patching.\"\n}\n```",
        "```json\n{\n  \"action\": \"objectives\",\n  \"op\": \"read\",\n  \"observation\": \"Load non-completed objectives for planning context.\",\n  \"rationale\": \"Need current objectives without completed items.\"\n}\n```",
        "```json\n{\n  \"action\": \"apply_patch\",\n  \"patch\": \"*** Begin Patch\\n*** Update File: path/to/file.rs\\n@@\\n- old\\n+ new\\n*** End Patch\",\n  \"observation\": \"Apply the required edit.\",\n  \"rationale\": \"Implement the change directly.\"\n}\n```",
        "```json\n{\n  \"action\": \"plan\",\n  \"op\": \"create_task\",\n  \"task\": {\"id\": \"T4\", \"title\": \"Add plan DAG\", \"status\": \"todo\", \"priority\": 3},\n  \"observation\": \"Planning update needed.\",\n  \"rationale\": \"Track work in PLAN.json via plan tool.\"\n}\n```",
        "```json\n{\n  \"action\": \"run_command\",\n  \"cmd\": \"rg -n \\\"trigger_observe\\\" canon-utils/canon-loop/src/executor.rs\",\n  \"observation\": \"Search for observe triggers.\",\n  \"rationale\": \"Locate all callsites before patching.\"\n}\n```",
        "```json\n{\n  \"action\": \"python\",\n  \"code\": \"import json; print('analyze')\",\n  \"observation\": \"Run structured analysis.\",\n  \"rationale\": \"Use Python for parsing tasks.\"\n}\n```",
        "```json\n{\n  \"action\": \"cargo_test\",\n  \"crate\": \"canon-runtime\",\n  \"test\": \"optional_test_name\",\n  \"observation\": \"Run tests for the target crate.\",\n  \"rationale\": \"Verify changes.\"\n}\n```",
        "```json\n{\n  \"action\": \"rustc_hir\",\n  \"crate\": \"canon-runtime\",\n  \"mode\": \"hir-tree\",\n  \"extra\": \"\",\n  \"observation\": \"Inspect HIR output.\",\n  \"rationale\": \"Diagnose compiler-level behavior.\"\n}\n```",
        "```json\n{\n  \"action\": \"rustc_mir\",\n  \"crate\": \"canon-runtime\",\n  \"mode\": \"mir\",\n  \"extra\": \"\",\n  \"observation\": \"Inspect MIR output.\",\n  \"rationale\": \"Diagnose compiler-level behavior.\"\n}\n```",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<String>>();
    templates.extend(GRAPH_TOOL_TEMPLATES.iter().map(|s| s.to_string()));
    let message_example = format!(
        "{}\n{}",
        format_message_schema("executor", "verifier", "result", "complete", &[]),
        blocker_message_example_template()
    );
    templates.push(message_example);
    templates
}

#[allow(dead_code)]
pub(crate) fn unsupported_action_correction(kind: &str) -> String {
    let mut msg = String::new();
    msg.push_str(&format!(
        "Invalid action: unsupported action '{kind}'.\nCorrective action required: use a supported tool action. Templates:\n"
    ));
    for template in unsupported_action_templates() {
        msg.push_str(&template);
        msg.push('\n');
    }
    msg.push_str("Return exactly one action.");
    msg
}

#[allow(dead_code)]
pub(crate) fn message_schema_correction(missing_field: &str, role: &str) -> String {
    let (from, to, msg_type, status) = default_message_route(role);
    let expected = expected_message_format(from, to, msg_type, status);
    let mut msg = String::new();
    msg.push_str(&format!(
        "Invalid action: message missing non-empty '{missing_field}'.\nCorrective action required: use a full message schema with required fields. Do not use `content`; use `payload`.\nTemplate:\n"
    ));
    msg.push_str(&format!(
        "{}\nReturn exactly one action.",
        format_message_schema(
            from,
            to,
            msg_type,
            status,
            &[
                ("details", "Optional extra context."),
                ("expected_format", expected.as_str())
            ]
        )
    ));
    msg
}

#[allow(dead_code)]
pub(crate) fn corrective_invalid_action_prompt(
    action: &Value,
    err_text: &str,
    role: &str,
) -> Option<String> {
    if err_text.contains("unsupported action") {
        let kind = action
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Some(unsupported_action_correction(kind));
    }
    if let Some(field) = err_text
        .strip_prefix("message missing non-empty '")
        .and_then(|rest| rest.strip_suffix("'"))
    {
        return Some(message_schema_correction(field, role));
    }
    if err_text.contains("message missing object payload") {
        return Some(message_schema_correction("payload", role));
    }
    None
}

pub(crate) fn invalid_action_expected_fields(kind: &str) -> Vec<&'static str> {
    match kind {
        "run_command" => vec!["action", "cmd", "rationale", "predicted_next_actions"],
        "read_file" => vec!["action", "path", "rationale", "predicted_next_actions"],
        "apply_patch" => vec!["action", "patch", "question", "rationale", "predicted_next_actions"],
        "cargo_test" => vec!["action", "crate", "rationale", "predicted_next_actions"],
        "list_dir" => vec!["action", "rationale", "predicted_next_actions"],
        "python" => vec!["action", "code", "rationale", "predicted_next_actions"],
        "plan" => vec!["action", "op", "question", "rationale", "predicted_next_actions"],
        "objectives" => vec!["action", "question", "rationale", "predicted_next_actions"],
        "message" => vec![
            "action",
            "from",
            "to",
            "type",
            "status",
            "payload",
            "rationale",
            "predicted_next_actions",
        ],
        "issue" => vec!["action", "op", "question", "rationale", "predicted_next_actions"],
        _ => vec!["action", "rationale", "predicted_next_actions"],
    }
}

fn example_predicted_next_actions() -> Value {
    json!([
        {
            "action": "read_file",
            "intent": "Inspect the relevant source before making changes."
        },
        {
            "action": "run_command",
            "intent": "Verify the current workspace state after the read."
        }
    ])
}

fn example_action_base(observation: &str, rationale: &str) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert(
        "observation".to_string(),
        Value::String(observation.to_string()),
    );
    map.insert(
        "rationale".to_string(),
        Value::String(rationale.to_string()),
    );
    map.insert(
        "predicted_next_actions".to_string(),
        example_predicted_next_actions(),
    );
    map
}

fn example_action_with_string_field(
    action: &str,
    field: &str,
    value: &str,
    observation: &str,
    rationale: &str,
) -> Value {
    let mut map = example_action_base(observation, rationale);
    map.insert("action".to_string(), Value::String(action.to_string()));
    map.insert(field.to_string(), Value::String(value.to_string()));
    Value::Object(map)
}

fn example_action_with_string_fields(
    action: &str,
    fields: &[(&str, &str)],
    observation: &str,
    rationale: &str,
) -> Value {
    let mut map = example_action_base(observation, rationale);
    map.insert("action".to_string(), Value::String(action.to_string()));
    for (field, value) in fields {
        map.insert((*field).to_string(), Value::String((*value).to_string()));
    }
    Value::Object(map)
}

fn example_plan_action() -> Value {
    json!({
        "action": "plan",
        "op": "create_task",
        "task": {
            "id": "T4",
            "title": "Add plan DAG",
            "status": "todo",
            "priority": 3
        },
        "observation": "Planning update needed.",
        "rationale": "Track work in PLAN.json via plan tool.",
        "predicted_next_actions": example_predicted_next_actions()
    })
}

fn example_message_action(from: &str, to: &str, msg_type: &str, status: &str) -> Value {
    let payload = if msg_type == "blocker" || status == "blocked" {
        json!({
            "summary": "Short blocker summary",
            "blocker": "Root cause",
            "evidence": "Concrete error text or failing command",
            "required_action": "What must be done to unblock",
            "severity": "error"
        })
    } else {
        json!({
            "summary": "Short summary"
        })
    };
    json!({
        "action": "message",
        "from": from,
        "to": to,
        "type": msg_type,
        "status": status,
        "observation": "Summarize what happened.",
        "rationale": "Explain why this is the next step.",
        "predicted_next_actions": example_predicted_next_actions(),
        "payload": payload
    })
}

fn push_missing_string_payload_field(
    schema_diff: &mut Vec<String>,
    payload: Option<&serde_json::Map<String, Value>>,
    field: &str,
) {
    let value = payload.and_then(|p| p.get(field));
    if let Some(val) = value {
        if !val.is_string() {
            if !schema_diff
                .iter()
                .any(|s| s == &format!("field type mismatch: payload.{field} (expected string)"))
            {
                schema_diff.push(format!("field type mismatch: payload.{field} (expected string)"));
            }
            return;
        }
    }
    if value
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
        && !schema_diff
            .iter()
            .any(|s| s == &format!("payload.{field} missing"))
    {
        schema_diff.push(format!("payload.{field} missing"));
    }
}

fn push_missing_string_object_field(
    schema_diff: &mut Vec<String>,
    obj: Option<&serde_json::Map<String, Value>>,
    field: &str,
) {
    let value = obj.and_then(|o| o.get(field));
    if let Some(val) = value {
        if let Some(text) = val.as_str() {
            if text.trim().is_empty()
                && !schema_diff
                    .iter()
                    .any(|s| s == &format!("missing field: {field}"))
            {
                schema_diff.push(format!("missing field: {field}"));
            }
        } else if !schema_diff
            .iter()
            .any(|s| s == &format!("field type mismatch: {field} (expected string)"))
        {
            schema_diff.push(format!("field type mismatch: {field} (expected string)"));
        }
    } else if !schema_diff
        .iter()
        .any(|s| s == &format!("missing field: {field}"))
    {
        schema_diff.push(format!("missing field: {field}"));
    }
}

fn push_missing_message_required_fields(
    schema_diff: &mut Vec<String>,
    obj: &serde_json::Map<String, Value>,
) {
    for field in ["from", "to", "type", "status", "payload", "rationale"] {
        let value = obj.get(field);
        let missing = match field {
            "payload" => value.and_then(|v| v.as_object()).is_none(),
            _ => value
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none(),
        };
        if missing {
            if !schema_diff.iter().any(|s| s == &format!("missing field: {field}")) {
                schema_diff.push(format!("missing field: {field}"));
            }
        }
        if field == "payload" {
            if let Some(val) = value {
                if !val.is_object() {
                    if !schema_diff.iter().any(|s| s == "payload must be object") {
                        schema_diff.push("payload must be object".to_string());
                    }
                    if !schema_diff
                        .iter()
                        .any(|s| s == "field type mismatch: payload (expected object)")
                    {
                        schema_diff.push(
                            "field type mismatch: payload (expected object)".to_string(),
                        );
                    }
                }
            }
        }
    }
}

fn example_action_for(kind: &str, role: &str, raw_action: Option<&Value>) -> Value {
    match kind {
        "run_command" => example_action_with_string_field(
            "run_command",
            "cmd",
            "rg -n \"pattern\" src/",
            "Search for the relevant code.",
            "Locate the target before patching.",
        ),
        "read_file" => example_action_with_string_field(
            "read_file",
            "path",
            "src/lib.rs",
            "Read the file for context.",
            "Need context before editing.",
        ),
        "list_dir" => example_action_with_string_field(
            "list_dir",
            "path",
            ".",
            "List workspace files.",
            "Locate the target before acting.",
        ),
        "apply_patch" => example_action_with_string_field(
            "apply_patch",
            "patch",
            "*** Begin Patch\n*** Update File: path/to/file.rs\n@@\n- old\n+ new\n*** End Patch",
            "Apply the requested change.",
            "Implement the edit directly.",
        ),
        "python" => example_action_with_string_field(
            "python",
            "code",
            "print('analysis')",
            "Run structured analysis.",
            "Use Python for parsing tasks.",
        ),
        "cargo_test" => example_action_with_string_fields(
            "cargo_test",
            &[("crate", "canon-mini-agent"), ("test", "optional_test_name")],
            "Run the targeted test.",
            "Verify the change.",
        ),
        "plan" => example_plan_action(),
        "message" => {
            let (from, to, msg_type, status) = normalized_message_example_route(raw_action, role);
            example_message_action(&from, &to, &msg_type, &status)
        }
        _ => example_message_action("executor", "verifier", "handoff", "complete"),
    }
}

fn normalized_message_example_route(raw_action: Option<&Value>, role: &str) -> (String, String, String, String) {
    raw_action
        .and_then(|action| action.as_object())
        .and_then(MessageRoute::from_action_obj)
        .map(MessageRoute::into_owned_lowercase)
        .unwrap_or_else(|| MessageRoute::default_for_role(role).into_owned_lowercase())
}

pub fn build_invalid_action_feedback(raw_action: Option<&Value>, err_text: &str, role: &str) -> String {
    let mut schema_diff: Vec<String> = Vec::new();
    let mut expected_fields: Vec<&'static str> = Vec::new();
    let mut expected_format: Option<String> = None;
    let mut example_action: Option<Value> = None;
    let mut action_schema: Option<String> = None;
    let push_unique = |schema_diff: &mut Vec<String>, msg: String| {
        if !schema_diff.iter().any(|s| s == &msg) {
            schema_diff.push(msg);
        }
    };
    if let Some(action) = raw_action {
        let obj = action.as_object();
        let kind = obj
            .and_then(|o| o.get("action"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        expected_fields = invalid_action_expected_fields(kind);
        example_action = Some(example_action_for(kind, role, Some(action)));
        action_schema = action_schema_json(kind);
        schema_diff = schema_diff_messages(action);
        if let Some(obj) = obj {
            if let Some(value) = obj.get("action") {
                if !value.is_string()
                    && !schema_diff
                        .iter()
                        .any(|s| s.starts_with("field type mismatch: action"))
                {
                    push_unique(
                        &mut schema_diff,
                        "field type mismatch: action (expected string)".to_string(),
                    );
                }
            }
            if let Some(rationale) = obj.get("rationale") {
                if let Some(text) = rationale.as_str() {
                    if text.trim().is_empty()
                        && !schema_diff.iter().any(|s| s == "missing field: rationale")
                    {
                        push_unique(&mut schema_diff, "missing field: rationale".to_string());
                    }
                } else {
                    push_unique(
                        &mut schema_diff,
                        "field type mismatch: rationale (expected string)".to_string(),
                    );
                }
            }
        }
        if let Some(observation) = action.get("observation") {
            if !observation.is_string()
                && !schema_diff
                    .iter()
                    .any(|s| s.starts_with("field type mismatch: observation"))
            {
                push_unique(
                    &mut schema_diff,
                    "field type mismatch: observation (expected string)".to_string(),
                );
            }
        }
        if kind == "apply_patch" {
            if let Some(patch) = action.get("patch").and_then(|v| v.as_str()) {
                if let Some(msg) = crate::tools::patch_scope_error(role, patch) {
                    push_unique(&mut schema_diff, msg);
                }
            }
        }
        if obj
            .and_then(|o| o.get("action"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            push_unique(&mut schema_diff, "missing field: action".to_string());
            push_unique(
                &mut schema_diff,
                "unsupported action: missing or unknown action".to_string(),
            );
        }
        if matches!(kind, "run_command" | "read_file" | "apply_patch" | "python" | "cargo_test") {
            let field = match kind {
                "run_command" => "cmd",
                "read_file" => "path",
                "apply_patch" => "patch",
                "python" => "code",
                "cargo_test" => "crate",
                _ => "",
            };
            push_missing_string_object_field(&mut schema_diff, obj, field);
        }
        if kind == "plan" && matches!(role, "planner" | "mini_planner") {
            let rationale = action.get("rationale").and_then(|v| v.as_str()).unwrap_or("");
            let observation = action.get("observation").and_then(|v| v.as_str()).unwrap_or("");
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
                let msg = "planner plan actions that rely on diagnostics must cite current source validation in observation/rationale (for example read_file, run_command, or verified source evidence)";
                push_unique(&mut schema_diff, msg.to_string());
            }
        }
        if kind == "message" {
            if let Some(obj) = obj {
                push_missing_message_required_fields(&mut schema_diff, obj);
            }
            if let Err(err) = validate_message_action(action, MessageValidationMode::Strict) {
                let msg = err.to_string();
                push_unique(&mut schema_diff, msg);
            }
            if let Some(obj) = obj {
                let get_str = |field: &str| obj.get(field).and_then(|v| v.as_str());
                let mut msg_type: Option<String> = None;
                let mut msg_status: Option<String> = None;
                for field in ["from", "to", "type", "status"] {
                    if let Some(val) = get_str(field) {
                        if val != val.to_lowercase() {
                            push_unique(&mut schema_diff, format!("role casing invalid: {field}={val}"));
                        }
                        if field == "type" {
                            msg_type = Some(val.to_string());
                        } else if field == "status" {
                            msg_status = Some(val.to_string());
                        }
                    }
                }
                if let (Some(msg_type), Some(msg_status)) = (msg_type.as_deref(), msg_status.as_deref()) {
                    let type_is_blocker = msg_type.eq_ignore_ascii_case("blocker");
                    let status_is_blocked = msg_status.eq_ignore_ascii_case("blocked");
                    if type_is_blocker != status_is_blocked {
                        push_unique(
                            &mut schema_diff,
                            format!("type/status mismatch: type={msg_type} status={msg_status}"),
                        );
                    }
                    let expected = expected_message_format(
                        get_str("from").unwrap_or("executor"),
                        get_str("to").unwrap_or("verifier"),
                        &msg_type,
                        &msg_status,
                    );
                    expected_format = Some(expected);
                }
                let payload = obj.get("payload").and_then(|v| v.as_object());
                if payload
                    .and_then(|p| p.get("summary"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .is_none()
                {
                    push_unique(&mut schema_diff, "payload.summary missing".to_string());
                }
                if obj.contains_key("blocker")
                    || obj.contains_key("evidence")
                    || obj.contains_key("required_action")
                {
                    push_unique(
                        &mut schema_diff,
                        "blocker fields must be inside payload: blocker/evidence/required_action"
                            .to_string(),
                    );
                }
                let is_blocker = msg_type
                    .as_deref()
                    .map(|v| v.eq_ignore_ascii_case("blocker"))
                    .unwrap_or(false)
                    || msg_status
                        .as_deref()
                        .map(|v| v.eq_ignore_ascii_case("blocked"))
                        .unwrap_or(false);
                if is_blocker {
                    for field in ["blocker", "evidence", "required_action"] {
                        push_missing_string_payload_field(&mut schema_diff, payload, field);
                    }
                }
            }
        }
    }
    let feedback = json!({
        "error_type": "invalid_action",
        "reason": err_text,
        "expected_fields": expected_fields,
        "schema_diff": schema_diff,
        "action_schema": action_schema,
        "expected_format": expected_format,
        "example_action": example_action,
    });
    format!(
        "Invalid action rejected.\naction_result:\n{}\nReturn exactly one action as a single JSON object in a ```json code block. No prose outside it.",
        feedback.to_string()
    )
}

pub fn auto_fill_message_fields(action: &mut Value, role: &str) -> bool {
    let obj = match action.as_object_mut() {
        Some(obj) => obj,
        None => return false,
    };
    if obj.get("action").and_then(|v| v.as_str()) != Some("message") {
        return false;
    }
    let defaults = MessageRoute::default_for_role(role);
    let default_from = defaults.from;
    let default_to = defaults.to;
    let default_type = defaults.msg_type;
    let default_status = defaults.status;
    let mut changed = false;
    if obj
        .get("from")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        obj.insert("from".to_string(), Value::String(default_from.to_string()));
        changed = true;
    }
    if obj
        .get("to")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        obj.insert("to".to_string(), Value::String(default_to.to_string()));
        changed = true;
    }
    if obj
        .get("type")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        obj.insert("type".to_string(), Value::String(default_type.to_string()));
        changed = true;
    }
    if obj
        .get("status")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        obj.insert(
            "status".to_string(),
            Value::String(default_status.to_string()),
        );
        changed = true;
    }
    if obj.get("payload").and_then(|v| v.as_object()).is_none() {
        obj.insert("payload".to_string(), json!({}));
        changed = true;
    }
    let from_val = obj
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or(default_from)
        .to_string();
    let to_val = obj
        .get("to")
        .and_then(|v| v.as_str())
        .unwrap_or(default_to)
        .to_string();
    let type_val = obj
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or(default_type)
        .to_string();
    let status_val = obj
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or(default_status)
        .to_string();
    let is_blocker = type_val.eq_ignore_ascii_case("blocker")
        || status_val.eq_ignore_ascii_case("blocked");

    if obj
        .get("predicted_next_actions")
        .and_then(|v| v.as_array())
        .filter(|items| (2..=3).contains(&items.len()))
        .is_none()
    {
        obj.insert(
            "predicted_next_actions".to_string(),
            example_predicted_next_actions(),
        );
        changed = true;
    }

    if let Some(payload) = obj.get_mut("payload").and_then(|v| v.as_object_mut()) {
        if payload
            .get("summary")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            payload.insert(
                "summary".to_string(),
                Value::String("auto-filled message fields".to_string()),
            );
            changed = true;
        }
        if payload
            .get("expected_format")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            payload.insert(
                "expected_format".to_string(),
                Value::String(expected_message_format(
                    &from_val,
                    &to_val,
                    &type_val,
                    &status_val,
                )),
            );
            changed = true;
        }
        if is_blocker {
            for (field, value) in [
                ("blocker", "auto-filled blocker details"),
                ("evidence", "auto-filled blocker evidence"),
                ("required_action", "auto-filled required action"),
            ] {
                if payload
                    .get(field)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .is_none()
                {
                    payload.insert(field.to_string(), Value::String(value.to_string()));
                    changed = true;
                }
            }
        }
    }

    if obj
        .get("observation")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        obj.insert(
            "observation".to_string(),
            Value::String("Auto-filled missing message fields.".to_string()),
        );
        changed = true;
    }
    if obj
        .get("rationale")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        obj.insert(
            "rationale".to_string(),
            Value::String("Repair invalid message schema to continue execution.".to_string()),
        );
        changed = true;
    }
    changed
}
