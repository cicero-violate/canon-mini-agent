use serde_json::{json, Value};

pub fn default_message_route(
    role: &str,
) -> (&'static str, &'static str, &'static str, &'static str) {
    if role.starts_with("executor") {
        ("executor", "verifier", "handoff", "complete")
    } else if role == "verifier" {
        ("verifier", "planner", "verification", "verified")
    } else if role == "diagnostics" {
        ("diagnostics", "planner", "diagnostics", "complete")
    } else if role == "planner" || role == "mini_planner" {
        ("planner", "executor", "plan", "complete")
    } else {
        ("executor", "verifier", "handoff", "complete")
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

#[allow(dead_code)]
pub(crate) fn unsupported_action_templates() -> Vec<String> {
    let mut templates = vec![
        "```json\n{\n  \"action\": \"list_dir\",\n  \"path\": \".\",\n  \"observation\": \"List workspace root to locate targets.\",\n  \"rationale\": \"Confirm files before acting.\"\n}\n```",
        "```json\n{\n  \"action\": \"read_file\",\n  \"path\": \"canon-utils/canon-loop/src/executor.rs\",\n  \"observation\": \"Read file to understand current logic.\",\n  \"rationale\": \"Need context before patching.\"\n}\n```",
        "```json\n{\n  \"action\": \"apply_patch\",\n  \"patch\": \"*** Begin Patch\\n*** Update File: path/to/file.rs\\n@@\\n- old\\n+ new\\n*** End Patch\",\n  \"observation\": \"Apply the required edit.\",\n  \"rationale\": \"Implement the change directly.\"\n}\n```",
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
        "```json\n{\n  \"action\": \"message\",\n  \"from\": \"executor\",\n  \"to\": \"planner\",\n  \"type\": \"blocker\",\n  \"status\": \"blocked\",\n  \"observation\": \"Describe the blocked state.\",\n  \"rationale\": \"Explain why progress is impossible without external action.\",\n  \"payload\": {\n    \"summary\": \"Short blocker summary\",\n    \"blocker\": \"Root cause\",\n    \"evidence\": \"Concrete error text or failing command\",\n    \"required_action\": \"What must be done to unblock\",\n    \"severity\": \"error\"\n  }\n}\n```"
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
        "run_command" => vec!["action", "cmd", "observation", "rationale"],
        "read_file" => vec!["action", "path", "observation", "rationale"],
        "apply_patch" => vec!["action", "patch", "observation", "rationale"],
        "cargo_test" => vec!["action", "crate", "observation", "rationale"],
        "list_dir" => vec!["action", "path", "observation", "rationale"],
        "python" => vec!["action", "code", "observation", "rationale"],
        "message" => vec![
            "action",
            "from",
            "to",
            "type",
            "status",
            "payload",
            "observation",
            "rationale",
        ],
        _ => vec!["action", "observation", "rationale"],
    }
}

fn is_supported_action(kind: &str) -> bool {
    matches!(
        kind,
        "run_command"
            | "read_file"
            | "list_dir"
            | "apply_patch"
            | "python"
            | "cargo_test"
            | "message"
    )
}

fn example_action_for(kind: &str, role: &str, raw_action: Option<&Value>) -> Value {
    match kind {
        "run_command" => json!({
            "action": "run_command",
            "cmd": "rg -n \"pattern\" src/",
            "observation": "Search for the relevant code.",
            "rationale": "Locate the target before patching."
        }),
        "read_file" => json!({
            "action": "read_file",
            "path": "src/lib.rs",
            "observation": "Read the file for context.",
            "rationale": "Need context before editing."
        }),
        "list_dir" => json!({
            "action": "list_dir",
            "path": ".",
            "observation": "List workspace files.",
            "rationale": "Locate the target before acting."
        }),
        "apply_patch" => json!({
            "action": "apply_patch",
            "patch": "*** Begin Patch\n*** Update File: path/to/file.rs\n@@\n- old\n+ new\n*** End Patch",
            "observation": "Apply the requested change.",
            "rationale": "Implement the edit directly."
        }),
        "python" => json!({
            "action": "python",
            "code": "print('analysis')",
            "observation": "Run structured analysis.",
            "rationale": "Use Python for parsing tasks."
        }),
        "cargo_test" => json!({
            "action": "cargo_test",
            "crate": "canon-mini-agent",
            "test": "optional_test_name",
            "observation": "Run the targeted test.",
            "rationale": "Verify the change."
        }),
        "message" => {
            let (from, to, msg_type, status) = raw_action
                .and_then(|action| action.as_object())
                .and_then(|obj| {
                    let from = obj.get("from").and_then(|v| v.as_str());
                    let to = obj.get("to").and_then(|v| v.as_str());
                    let msg_type = obj.get("type").and_then(|v| v.as_str());
                    let status = obj.get("status").and_then(|v| v.as_str());
                    match (from, to, msg_type, status) {
                        (Some(from), Some(to), Some(msg_type), Some(status)) => Some((
                            from.to_lowercase(),
                            to.to_lowercase(),
                            msg_type.to_lowercase(),
                            status.to_lowercase(),
                        )),
                        _ => None,
                    }
                })
                .map(|(from, to, msg_type, status)| (from, to, msg_type, status))
                .map(|(from, to, msg_type, status)| {
                    (from, to, msg_type, status)
                })
                .unwrap_or_else(|| {
                    let (from, to, msg_type, status) = default_message_route(role);
                    (
                        from.to_string(),
                        to.to_string(),
                        msg_type.to_string(),
                        status.to_string(),
                    )
                });
            json!({
                "action": "message",
                "from": from,
                "to": to,
                "type": msg_type,
                "status": status,
                "observation": "Summarize what happened.",
                "rationale": "Explain why this is the next step.",
                "payload": {
                    "summary": "Short summary"
                }
            })
        }
        _ => json!({
            "action": "message",
            "from": "executor",
            "to": "verifier",
            "type": "handoff",
            "status": "complete",
            "observation": "Summarize what happened.",
            "rationale": "Explain why this is the next step.",
            "payload": {
                "summary": "Short summary"
            }
        }),
    }
}

fn push_type_mismatch(
    schema_diff: &mut Vec<String>,
    obj: &serde_json::Map<String, Value>,
    field: &str,
    expected: &str,
) {
    if let Some(val) = obj.get(field) {
        let type_ok = match expected {
            "string" => val.is_string(),
            "object" => val.is_object(),
            _ => true,
        };
        if !type_ok {
            schema_diff.push(format!("field type mismatch: {field} (expected {expected})"));
        }
    }
}

pub fn build_invalid_action_feedback(raw_action: Option<&Value>, err_text: &str, role: &str) -> String {
    let mut schema_diff: Vec<String> = Vec::new();
    let mut expected_fields: Vec<&'static str> = Vec::new();
    let mut expected_format: Option<String> = None;
    let mut example_action: Option<Value> = None;
    if let Some(action) = raw_action {
        let obj = action.as_object();
        let kind = obj
            .and_then(|o| o.get("action"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        expected_fields = invalid_action_expected_fields(kind);
        example_action = Some(example_action_for(kind, role, Some(action)));
        if let Some(obj) = obj {
            if obj
                .get("action")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none()
            {
                schema_diff.push("missing field: action".to_string());
            }
            push_type_mismatch(&mut schema_diff, obj, "action", "string");
            // observation is optional; only validate type if present
            if obj.contains_key("observation") {
                push_type_mismatch(&mut schema_diff, obj, "observation", "string");
            }
            if obj
                .get("rationale")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none()
            {
                schema_diff.push("missing field: rationale".to_string());
            }
            push_type_mismatch(&mut schema_diff, obj, "rationale", "string");
            if obj.get("action").is_none() || kind == "unknown" {
                schema_diff.push("unsupported action: missing or unknown action".to_string());
            } else if !is_supported_action(kind) {
                schema_diff.push(format!("unsupported action: {kind}"));
            }
            match kind {
                "run_command" => {
                    push_type_mismatch(&mut schema_diff, obj, "cmd", "string");
                    if obj
                        .get("cmd")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .is_none()
                    {
                        schema_diff.push("missing field: cmd".to_string());
                    }
                }
                "read_file" | "list_dir" => {
                    push_type_mismatch(&mut schema_diff, obj, "path", "string");
                    if obj
                        .get("path")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .is_none()
                    {
                        schema_diff.push("missing field: path".to_string());
                    }
                }
                "apply_patch" => {
                    push_type_mismatch(&mut schema_diff, obj, "patch", "string");
                    if obj
                        .get("patch")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .is_none()
                    {
                        schema_diff.push("missing field: patch".to_string());
                    }
                }
                "python" => {
                    push_type_mismatch(&mut schema_diff, obj, "code", "string");
                    if obj
                        .get("code")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .is_none()
                    {
                        schema_diff.push("missing field: code".to_string());
                    }
                }
                "cargo_test" => {
                    push_type_mismatch(&mut schema_diff, obj, "crate", "string");
                    if obj
                        .get("crate")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .is_none()
                    {
                        schema_diff.push("missing field: crate".to_string());
                    }
                }
                "message" => {
                    // ensure blocker payload fields are auto-filled if missing
                    if let Some(payload) = obj.get("payload").and_then(|v| v.as_object()) {
                        let mut payload = payload.clone();
                        let is_blocker = obj
                            .get("type")
                            .and_then(|v| v.as_str())
                            .map(|s| s == "blocker")
                            .unwrap_or(false)
                            || obj
                                .get("status")
                                .and_then(|v| v.as_str())
                                .map(|s| s == "blocked")
                                .unwrap_or(false);

                        if is_blocker {
                            if payload
                                .get("summary")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .is_none()
                            {
                                payload.insert("summary".to_string(), Value::String("auto-filled summary".to_string()));
                            }
                            if payload
                                .get("blocker")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .is_none()
                            {
                                payload.insert("blocker".to_string(), Value::String("auto-filled blocker".to_string()));
                            }
                            if payload
                                .get("evidence")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .is_none()
                            {
                                payload.insert("evidence".to_string(), Value::String("auto-filled evidence".to_string()));
                            }
                            if payload
                                .get("required_action")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .is_none()
                            {
                                payload.insert("required_action".to_string(), Value::String("auto-filled required action".to_string()));
                            }
                        }
                    }
                    let mut msg_type: Option<String> = None;
                    let mut msg_status: Option<String> = None;
                    for field in ["from", "to", "type", "status"] {
                        push_type_mismatch(&mut schema_diff, obj, field, "string");
                        if let Some(val) = obj.get(field).and_then(|v| v.as_str()) {
                            if val != val.to_lowercase() {
                                schema_diff.push(format!("role casing invalid: {field}={val}"));
                            }
                            if field == "type" {
                                msg_type = Some(val.to_string());
                            } else if field == "status" {
                                msg_status = Some(val.to_string());
                            }
                        }
                        if obj
                            .get(field)
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .is_none()
                        {
                            schema_diff.push(format!("missing field: {field}"));
                        }
                    }
                    if let (Some(msg_type), Some(msg_status)) = (msg_type, msg_status) {
                        let type_is_blocker = msg_type.eq_ignore_ascii_case("blocker");
                        let status_is_blocked = msg_status.eq_ignore_ascii_case("blocked");
                        if type_is_blocker != status_is_blocked {
                            schema_diff.push(format!(
                                "type/status mismatch: type={msg_type} status={msg_status}"
                            ));
                        }
                        let expected = expected_message_format(
                            obj.get("from").and_then(|v| v.as_str()).unwrap_or("executor"),
                            obj.get("to").and_then(|v| v.as_str()).unwrap_or("verifier"),
                            &msg_type,
                            &msg_status,
                        );
                        expected_format = Some(expected);
                    }
                    if obj.get("payload").and_then(|v| v.as_object()).is_none() {
                        schema_diff.push("missing field: payload".to_string());
                        if obj.get("payload").is_some() {
                            schema_diff.push("payload must be object".to_string());
                        }
                    }
                    push_type_mismatch(&mut schema_diff, obj, "payload", "object");
                    let payload = obj.get("payload").and_then(|v| v.as_object());
                    if payload
                        .and_then(|p| p.get("summary"))
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .is_none()
                    {
                        schema_diff.push("payload.summary missing".to_string());
                    }
                    if obj.contains_key("blocker")
                        || obj.contains_key("evidence")
                        || obj.contains_key("required_action")
                    {
                        schema_diff.push(
                            "blocker fields must be inside payload: blocker/evidence/required_action"
                                .to_string(),
                        );
                    }
                    let is_blocker = obj
                        .get("type")
                        .and_then(|v| v.as_str())
                        .map(|v| v.eq_ignore_ascii_case("blocker"))
                        .unwrap_or(false)
                        || obj
                            .get("status")
                            .and_then(|v| v.as_str())
                            .map(|v| v.eq_ignore_ascii_case("blocked"))
                            .unwrap_or(false);
                    if is_blocker {
                        for field in ["blocker", "evidence", "required_action"] {
                            let value = payload.and_then(|p| p.get(field));
                            if let Some(val) = value {
                                if !val.is_string() {
                                    schema_diff.push(format!(
                                        "field type mismatch: payload.{field} (expected string)"
                                    ));
                                    continue;
                                }
                            }
                            if value
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .is_none()
                            {
                                schema_diff.push(format!("payload.{field} missing"));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let feedback = json!({
        "error_type": "invalid_action",
        "reason": err_text,
        "expected_fields": expected_fields,
        "schema_diff": schema_diff,
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
    let (default_from, default_to, default_type, default_status) = default_message_route(role);
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
    if changed {
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
        if let Some(payload) = obj.get_mut("payload").and_then(|v| v.as_object_mut()) {
            payload
                .entry("summary")
                .or_insert(Value::String("auto-filled message fields".to_string()));
            payload
                .entry("expected_format")
                .or_insert(Value::String(expected_message_format(
                    &from_val,
                    &to_val,
                    &type_val,
                    &status_val,
                )));
            if is_blocker {
                payload
                    .entry("blocker")
                    .or_insert(Value::String("auto-filled blocker details".to_string()));
                payload
                    .entry("evidence")
                    .or_insert(Value::String("auto-filled blocker evidence".to_string()));
                payload.entry("required_action").or_insert(Value::String(
                    "auto-filled required action".to_string(),
                ));
            }
        }
        obj.entry("observation".to_string())
            .or_insert(Value::String(
                "Auto-filled missing message fields.".to_string(),
            ));
        obj.entry("rationale".to_string()).or_insert(Value::String(
            "Repair invalid message schema to continue execution.".to_string(),
        ));
    }
    changed
}
