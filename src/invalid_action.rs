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
    to_role: &'a str,
    msg_type: &'a str,
    status: &'a str,
}

impl<'a> MessageRoute<'a> {
    fn default_for_role(role: &'a str) -> Self {
        let (from, to_role, msg_type, status) = default_message_route(role);
        Self {
            from,
            to_role,
            msg_type,
            status,
        }
    }

    fn from_action_obj(obj: &'a serde_json::Map<String, Value>) -> Option<Self> {
        let from = obj.get("from").and_then(|v| v.as_str())?;
        let to_role = obj.get("to").and_then(|v| v.as_str())?;
        let msg_type = obj.get("type").and_then(|v| v.as_str())?;
        let status = obj.get("status").and_then(|v| v.as_str())?;
        Some(Self {
            from,
            to_role,
            msg_type,
            status,
        })
    }

    fn into_owned_lowercase(self) -> (String, String, String, String) {
        (
            self.from.to_lowercase(),
            self.to_role.to_lowercase(),
            self.msg_type.to_lowercase(),
            self.status.to_lowercase(),
        )
    }
}

pub fn expected_message_format(
    from: &str,
    to_role: &str,
    msg_type: &str,
    status: &str,
) -> String {
    format!(
        "{{ \"action\": \"message\", \"from\": \"{from}\", \"to\": \"{to_role}\", \"type\": \"{msg_type}\", \"status\": \"{status}\", \"payload\": {{ \"summary\": \"...\" }} }}"
    )
}

pub fn format_message_schema(
    from: &str,
    to_role: &str,
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
        "```json\n{{\n  \"action\": \"message\",\n  \"from\": \"{from}\",\n  \"to\": \"{to_role}\",\n  \"type\": \"{msg_type}\",\n  \"status\": \"{status}\",\n  \"observation\": \"...\",\n  \"rationale\": \"...\",\n  \"predicted_next_actions\": [\n    {{\"action\": \"read_file\", \"intent\": \"Inspect the relevant source before making changes.\"}},\n    {{\"action\": \"run_command\", \"intent\": \"Verify the current workspace state after the read.\"}}\n  ],\n  \"payload\": {{\n    {payload_lines}\n  }}\n}}\n```"
    )
}

#[allow(dead_code)]
const GRAPH_TOOL_TEMPLATES: [&str; 4] = [
    "```json\n{\n  \"action\": \"graph_call\",\n  \"crate\": \"canon_mini_agent\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate call graph.\",\n  \"rationale\": \"Inspect call graph output.\"\n}\n```",
    "```json\n{\n  \"action\": \"graph_cfg\",\n  \"crate\": \"canon_mini_agent\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate CFG graph.\",\n  \"rationale\": \"Inspect CFG output.\"\n}\n```",
    "```json\n{\n  \"action\": \"graph_dataflow\",\n  \"crate\": \"canon_mini_agent\",\n  \"tlog\": \"\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate dataflow report.\",\n  \"rationale\": \"Inspect dataflow metrics.\"\n}\n```",
    "```json\n{\n  \"action\": \"graph_reachability\",\n  \"crate\": \"canon_mini_agent\",\n  \"tlog\": \"\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate reachability report.\",\n  \"rationale\": \"Inspect reachability metrics.\"\n}\n```",
];

fn blocker_message_example_template() -> &'static str {
    "```json\n{\n  \"action\": \"message\",\n  \"from\": \"executor\",\n  \"to\": \"planner\",\n  \"type\": \"blocker\",\n  \"status\": \"blocked\",\n  \"observation\": \"Describe the blocked state.\",\n  \"rationale\": \"Explain why progress is impossible without external action.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"read_file\", \"intent\": \"Inspect the relevant source before making changes.\"},\n    {\"action\": \"run_command\", \"intent\": \"Verify the current workspace state after the read.\"}\n  ],\n  \"payload\": {\n    \"summary\": \"Short blocker summary\",\n    \"blocker\": \"Root cause\",\n    \"evidence\": \"Concrete error text or failing command\",\n    \"required_action\": \"What must be done to unblock\",\n    \"severity\": \"error\"\n  }\n}\n```"
}

#[allow(dead_code)]
pub(crate) fn unsupported_action_templates() -> Vec<String> {
    let mut templates = vec![
        "```json\n{\n  \"action\": \"list_dir\",\n  \"path\": \".\",\n  \"observation\": \"List workspace root to locate targets.\",\n  \"rationale\": \"Confirm files before acting.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"read_file\", \"intent\": \"Open the most relevant file after locating it.\"},\n    {\"action\": \"run_command\", \"intent\": \"Verify workspace state after locating targets.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"read_file\",\n  \"path\": \"src/lib.rs\",\n  \"observation\": \"Read file to understand current logic.\",\n  \"rationale\": \"Need context before patching.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"symbol_window\", \"intent\": \"Inspect the exact function body after the read.\"},\n    {\"action\": \"apply_patch\", \"intent\": \"Patch the file once the relevant context is confirmed.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"symbols_index\",\n  \"path\": \"src\",\n  \"out\": \"state/symbols.json\",\n  \"observation\": \"Build deterministic symbol inventory.\",\n  \"rationale\": \"Need a unique sorted symbols catalog before rename/refactor planning.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"read_file\", \"intent\": \"Inspect generated symbols catalog.\"},\n    {\"action\": \"rename_symbol\", \"intent\": \"Rename one selected symbol precisely.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"symbols_rename_candidates\",\n  \"symbols_path\": \"state/symbols.json\",\n  \"out\": \"state/rename_candidates.json\",\n  \"observation\": \"Derive deterministic rename candidates from symbol inventory.\",\n  \"rationale\": \"Prioritize naming cleanup before direct symbol mutation.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"read_file\", \"intent\": \"Inspect generated rename candidates.\"},\n    {\"action\": \"rename_symbol\", \"intent\": \"Apply one selected rename candidate.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"symbols_prepare_rename\",\n  \"candidates_path\": \"state/rename_candidates.json\",\n  \"index\": 0,\n  \"out\": \"state/next_rename_action.json\",\n  \"observation\": \"Select the top rename candidate.\",\n  \"rationale\": \"Prepare a concrete rename_symbol payload before mutation.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"read_file\", \"intent\": \"Inspect prepared rename action JSON.\"},\n    {\"action\": \"rename_symbol\", \"intent\": \"Execute the prepared rename action.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"rename_symbol\",\n  \"old_symbol\": \"tools::handle_plan_action\",\n  \"new_symbol\": \"tools::handle_master_plan_action\",\n  \"question\": \"Is this the exact symbol that should be renamed across the crate without changing behavior?\",\n  \"observation\": \"Source evidence identified the target symbol.\",\n  \"rationale\": \"Perform a deterministic symbol rename.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"cargo_test\", \"intent\": \"Run focused tests for renamed behavior.\"},\n    {\"action\": \"run_command\", \"intent\": \"Run cargo check for compile safety.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"objectives\",\n  \"op\": \"read\",\n  \"observation\": \"Load non-completed objectives for planning context.\",\n  \"rationale\": \"Need current objectives without completed items.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"plan\", \"intent\": \"Align plan tasks with active objectives.\"},\n    {\"action\": \"read_file\", \"intent\": \"Inspect the source tied to the next actionable objective.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"apply_patch\",\n  \"patch\": \"*** Begin Patch\\n*** Update File: path/to/file.rs\\n@@\\n- old\\n+ new\\n*** End Patch\",\n  \"question\": \"Does current source evidence justify this exact file edit?\",\n  \"observation\": \"Apply the required edit.\",\n  \"rationale\": \"Implement the change directly.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"cargo_test\", \"intent\": \"Verify the patch with tests.\"},\n    {\"action\": \"run_command\", \"intent\": \"Check the workspace for follow-on failures.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"plan\",\n  \"op\": \"create_task\",\n  \"question\": \"Does the current evidence require a new tracked task in PLAN.json?\",\n  \"task\": {\"id\": \"T4\", \"title\": \"Add plan DAG\", \"status\": \"todo\", \"priority\": 3},\n  \"observation\": \"Planning update needed.\",\n  \"rationale\": \"Track work in PLAN.json via plan tool.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"read_file\", \"intent\": \"Inspect the source tied to the new task.\"},\n    {\"action\": \"run_command\", \"intent\": \"Gather evidence for the next task step.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"run_command\",\n  \"cmd\": \"rg -n \\\"trigger_observe\\\" src\",\n  \"observation\": \"Search for observe triggers.\",\n  \"rationale\": \"Locate all callsites before patching.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"read_file\", \"intent\": \"Open the matching source region.\"},\n    {\"action\": \"apply_patch\", \"intent\": \"Patch the confirmed callsite if needed.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"python\",\n  \"code\": \"import json; print('analyze')\",\n  \"observation\": \"Run structured analysis.\",\n  \"rationale\": \"Use Python for parsing tasks.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"read_file\", \"intent\": \"Inspect files identified by the analysis.\"},\n    {\"action\": \"run_command\", \"intent\": \"Verify the analysis result in the workspace.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"cargo_test\",\n  \"crate\": \"canon-mini-agent\",\n  \"test\": \"optional_test_name\",\n  \"observation\": \"Run tests for the target crate.\",\n  \"rationale\": \"Verify changes.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"read_file\", \"intent\": \"Inspect source tied to any failing test.\"},\n    {\"action\": \"run_command\", \"intent\": \"Review the generated test log if more detail is needed.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"rustc_hir\",\n  \"crate\": \"canon_mini_agent\",\n  \"mode\": \"hir-tree\",\n  \"extra\": \"\",\n  \"observation\": \"Inspect HIR output.\",\n  \"rationale\": \"Diagnose compiler-level behavior.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"symbol_window\", \"intent\": \"Inspect the concrete symbol body after the HIR scan.\"},\n    {\"action\": \"read_file\", \"intent\": \"Open the relevant source lines once the target symbol is known.\"}\n  ]\n}\n```",
        "```json\n{\n  \"action\": \"rustc_mir\",\n  \"crate\": \"canon_mini_agent\",\n  \"mode\": \"mir\",\n  \"extra\": \"\",\n  \"observation\": \"Inspect MIR output.\",\n  \"rationale\": \"Diagnose compiler-level behavior.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"symbol_neighborhood\", \"intent\": \"Inspect nearby callers and callees after the MIR scan.\"},\n    {\"action\": \"read_file\", \"intent\": \"Open the relevant source lines once the MIR target is confirmed.\"}\n  ]\n}\n```",
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
    msg.push_str(
        "For any mutating retry (`apply_patch`, `plan`, `objectives`, `issue`, or `rename_symbol`), include a non-empty `question` field stating the decision-boundary premise. Return exactly one action."
    );
    msg
}

#[allow(dead_code)]
pub(crate) fn message_schema_correction(missing_field: &str, role: &str) -> String {
    let (from, to_role, msg_type, status) = default_message_route(role);
    let expected = expected_message_format(from, to_role, msg_type, status);
    let mut msg = String::new();
    msg.push_str(&format!(
        "Invalid action: message missing non-empty '{missing_field}'.\nCorrective action required: use a full message schema with required fields. Do not use `content`; use `payload`.\nTemplate:\n"
    ));
    msg.push_str(&format!(
        "{}\nFor any mutating retry (`apply_patch`, `plan`, `objectives`, `issue`, or `rename_symbol`), include a non-empty `question` field stating the decision-boundary premise. Return exactly one action.",
        format_message_schema(
            from,
            to_role,
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

const BASIC_ACTION_FIELDS: &[&str] = &["action", "rationale", "predicted_next_actions"];
const QUESTION_ACTION_FIELDS: &[&str] = &[
    "action",
    "question",
    "rationale",
    "predicted_next_actions",
];

pub(crate) fn invalid_action_expected_fields(kind: &str) -> Vec<&'static str> {
    match kind {
        "run_command" => vec!["action", "cmd", "rationale", "predicted_next_actions"],
        "read_file" => vec!["action", "path", "rationale", "predicted_next_actions"],
        "symbols_index" | "symbols_rename_candidates" | "symbols_prepare_rename" | "list_dir" => {
            BASIC_ACTION_FIELDS.to_vec()
        }
        "rename_symbol" => vec![
            "action",
            "old_symbol",
            "new_symbol",
            "question",
            "rationale",
            "predicted_next_actions",
        ],
        "apply_patch" => vec!["action", "patch", "question", "rationale", "predicted_next_actions"],
        "cargo_test" => vec!["action", "crate", "rationale", "predicted_next_actions"],
        "python" => vec!["action", "code", "rationale", "predicted_next_actions"],
        "plan" => vec!["action", "op", "question", "rationale", "predicted_next_actions"],
        "objectives" | "issue" => QUESTION_ACTION_FIELDS.to_vec(),
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
        _ => BASIC_ACTION_FIELDS.to_vec(),
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

fn example_message_payload(msg_type: &str, status: &str) -> Value {
    if msg_type == "blocker" || status == "blocked" {
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
    }
}

fn example_message_action(from: &str, to_role: &str, msg_type: &str, status: &str) -> Value {
    let payload = example_message_payload(msg_type, status);
    json!({
        "action": "message",
        "from": from,
        "to": to_role,
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

fn example_simple_action(kind: &str) -> Option<Value> {
    match kind {
        "run_command" => Some(example_action_with_string_field(
            "run_command",
            "cmd",
            "rg -n \"pattern\" src/",
            "Search for the relevant code.",
            "Locate the target before patching.",
        )),
        "read_file" => Some(example_action_with_string_field(
            "read_file",
            "path",
            "src/lib.rs",
            "Read the file for context.",
            "Need context before editing.",
        )),
        "list_dir" => Some(example_action_with_string_field(
            "list_dir",
            "path",
            ".",
            "List workspace files.",
            "Locate the target before acting.",
        )),
        "apply_patch" => Some(example_action_with_string_field(
            "apply_patch",
            "patch",
            "*** Begin Patch\n*** Update File: path/to/file.rs\n@@\n- old\n+ new\n*** End Patch",
            "Apply the requested change.",
            "Implement the edit directly.",
        )),
        "python" => Some(example_action_with_string_field(
            "python",
            "code",
            "print('analysis')",
            "Run structured analysis.",
            "Use Python for parsing tasks.",
        )),
        _ => None,
    }
}

fn example_symbol_workflow_action(kind: &str) -> Option<Value> {
    match kind {
        "symbols_index" => Some(build_symbol_workflow_example_action(
            "symbols_index",
            &[
                ("path", Value::String("src".to_string())),
                ("out", Value::String("state/symbols.json".to_string())),
            ],
            "Build deterministic symbol inventory.",
            "Need a unique sorted symbols catalog before rename/refactor planning.",
        )),
        "symbols_rename_candidates" => Some(build_symbol_workflow_example_action(
            "symbols_rename_candidates",
            &[
                (
                    "symbols_path",
                    Value::String("state/symbols.json".to_string()),
                ),
                (
                    "out",
                    Value::String("state/rename_candidates.json".to_string()),
                ),
            ],
            "Derive deterministic rename candidates from symbol inventory.",
            "Prioritize naming cleanup before direct symbol mutation.",
        )),
        "symbols_prepare_rename" => Some(build_symbol_workflow_example_action(
            "symbols_prepare_rename",
            &[
                (
                    "candidates_path",
                    Value::String("state/rename_candidates.json".to_string()),
                ),
                ("index", Value::from(0)),
                (
                    "out",
                    Value::String("state/next_rename_action.json".to_string()),
                ),
            ],
            "Select the top rename candidate.",
            "Prepare a concrete rename_symbol payload before mutation.",
        )),
        "rename_symbol" => Some(build_symbol_workflow_example_action(
            "rename_symbol",
            &[
                (
                    "old_symbol",
                    Value::String("tools::handle_plan_action".to_string()),
                ),
                (
                    "new_symbol",
                    Value::String("tools::handle_master_plan_action".to_string()),
                ),
                (
                    "question",
                    Value::String("Is this the exact symbol to rename across the crate?".to_string()),
                ),
            ],
            "Span-backed rename should update all references consistently.",
            "Perform a deterministic symbol rename using rustc graph spans.",
        )),
        _ => None,
    }
}

fn build_symbol_workflow_example_action(
    action: &str,
    fields: &[(&str, Value)],
    observation: &str,
    rationale: &str,
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("action".to_string(), Value::String(action.to_string()));
    for (field, value) in fields {
        obj.insert((*field).to_string(), value.clone());
    }
    obj.insert(
        "observation".to_string(),
        Value::String(observation.to_string()),
    );
    obj.insert(
        "rationale".to_string(),
        Value::String(rationale.to_string()),
    );
    obj.insert(
        "predicted_next_actions".to_string(),
        example_predicted_next_actions(),
    );
    Value::Object(obj)
}

fn example_action_for(kind: &str, role: &str, raw_action: Option<&Value>) -> Value {
    if let Some(action) = example_simple_action(kind) {
        return action;
    }
    if let Some(action) = example_symbol_workflow_action(kind) {
        return action;
    }

    match kind {
        "cargo_test" => example_action_with_string_fields(
            "cargo_test",
            &[("crate", "canon-mini-agent"), ("test", "optional_test_name")],
            "Run the targeted test.",
            "Verify the change.",
        ),
        "plan" => example_plan_action(),
        "message" => {
            let (from, to_role, msg_type, status) = normalized_message_example_route(raw_action, role);
            example_message_action(&from, &to_role, &msg_type, &status)
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
    let mut action_schema: Option<Value> = None;
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
        collect_general_action_schema_diff(&mut schema_diff, action, obj, role, kind);
        maybe_push_planner_plan_diagnostics_error(&mut schema_diff, action, role, kind);
        if kind == "message" {
            collect_message_schema_diff(&mut schema_diff, action, obj, &mut expected_format);
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
        "Invalid action rejected.\naction_result:\n{}\nReturn exactly one action as a single JSON object in a ```json code block. No prose outside it.\nFor any mutating retry (`apply_patch`, `plan`, `objectives`, `issue`, or `rename_symbol`), include a non-empty `question` field stating the decision-boundary premise.",
        serde_json::to_string_pretty(&feedback).unwrap_or_else(|_| feedback.to_string())
    )
}

fn collect_general_action_schema_diff(
    schema_diff: &mut Vec<String>,
    action: &Value,
    obj: Option<&serde_json::Map<String, Value>>,
    role: &str,
    kind: &str,
) {
    if let Some(obj) = obj {
        push_type_mismatch_if_present(schema_diff, obj.get("action"), "action");
        push_missing_or_invalid_rationale(schema_diff, obj.get("rationale"));
    }
    push_type_mismatch_if_present(schema_diff, action.get("observation"), "observation");
    if kind == "apply_patch" {
        if let Some(patch) = action.get("patch").and_then(|v| v.as_str()) {
            if let Some(msg) = crate::tools::patch_scope_error(role, patch) {
                add_unique_schema_diff(schema_diff, msg);
            }
        }
    }
    push_missing_or_unknown_action(schema_diff, obj);
    if matches!(kind, "run_command" | "read_file" | "apply_patch" | "python" | "cargo_test") {
        let field = match kind {
            "run_command" => "cmd",
            "read_file" => "path",
            "apply_patch" => "patch",
            "python" => "code",
            "cargo_test" => "crate",
            _ => "",
        };
        push_missing_string_object_field(schema_diff, obj, field);
    }
}

fn push_type_mismatch_if_present(
    schema_diff: &mut Vec<String>,
    value: Option<&Value>,
    field: &str,
) {
    if let Some(value) = value {
        if !value.is_string() {
            add_unique_schema_diff(
                schema_diff,
                format!("field type mismatch: {field} (expected string)"),
            );
        }
    }
}

fn push_missing_or_invalid_rationale(schema_diff: &mut Vec<String>, rationale: Option<&Value>) {
    match rationale.and_then(|value| value.as_str()) {
        Some(text) if !text.trim().is_empty() => {}
        Some(_) => add_unique_schema_diff(schema_diff, "missing field: rationale".to_string()),
        None => push_type_mismatch_if_present(schema_diff, rationale, "rationale"),
    }
}

fn push_missing_or_unknown_action(
    schema_diff: &mut Vec<String>,
    obj: Option<&serde_json::Map<String, Value>>,
) {
    let has_action = obj
        .and_then(|o| o.get("action"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();
    if !has_action {
        add_unique_schema_diff(schema_diff, "missing field: action".to_string());
        add_unique_schema_diff(
            schema_diff,
            "unsupported action: missing or unknown action".to_string(),
        );
    }
}

fn add_unique_schema_diff(schema_diff: &mut Vec<String>, msg: String) {
    if !schema_diff.iter().any(|s| s == &msg) {
        schema_diff.push(msg);
    }
}

fn maybe_push_planner_plan_diagnostics_error(
    schema_diff: &mut Vec<String>,
    action: &Value,
    role: &str,
    kind: &str,
) {
    if kind != "plan" || !matches!(role, "planner" | "mini_planner") {
        return;
    }
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
        add_unique_schema_diff(schema_diff, msg.to_string());
    }
}

fn collect_message_schema_diff(
    schema_diff: &mut Vec<String>,
    action: &Value,
    obj: Option<&serde_json::Map<String, Value>>,
    expected_format: &mut Option<String>,
) {
    if let Some(obj) = obj {
        push_missing_message_required_fields(schema_diff, obj);
    }
    if let Err(err) = validate_message_action(action, MessageValidationMode::Strict) {
        add_unique_schema_diff(schema_diff, err.to_string());
    }
    let Some(obj) = obj else {
        return;
    };
    let (msg_type, msg_status) = collect_message_route_fields(schema_diff, obj);
    if let (Some(msg_type), Some(msg_status)) = (msg_type.as_deref(), msg_status.as_deref()) {
        let type_is_blocker = msg_type.eq_ignore_ascii_case("blocker");
        let status_is_blocked = msg_status.eq_ignore_ascii_case("blocked");
        if type_is_blocker != status_is_blocked {
            add_unique_schema_diff(
                schema_diff,
                format!("type/status mismatch: type={msg_type} status={msg_status}"),
            );
        }
        *expected_format = Some(expected_message_format(
            message_field_str(obj, "from").unwrap_or("executor"),
            message_field_str(obj, "to").unwrap_or("verifier"),
            msg_type,
            msg_status,
        ));
    }
    let payload = obj.get("payload").and_then(|v| v.as_object());
    validate_message_payload_summary(schema_diff, payload);
    validate_blocker_payload_placement(schema_diff, obj);
    validate_blocker_payload_fields(schema_diff, payload, msg_type.as_deref(), msg_status.as_deref());
}

fn message_field_str<'a>(
    obj: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Option<&'a str> {
    obj.get(field).and_then(|v| v.as_str())
}

fn collect_message_route_fields(
    schema_diff: &mut Vec<String>,
    obj: &serde_json::Map<String, Value>,
) -> (Option<String>, Option<String>) {
    let mut msg_type: Option<String> = None;
    let mut msg_status: Option<String> = None;
    for field in ["from", "to", "type", "status"] {
        if let Some(val) = message_field_str(obj, field) {
            if val != val.to_lowercase() {
                add_unique_schema_diff(schema_diff, format!("role casing invalid: {field}={val}"));
            }
            if field == "type" {
                msg_type = Some(val.to_string());
            } else if field == "status" {
                msg_status = Some(val.to_string());
            }
        }
    }
    (msg_type, msg_status)
}

fn validate_message_payload_summary(
    schema_diff: &mut Vec<String>,
    payload: Option<&serde_json::Map<String, Value>>,
) {
    if payload
        .and_then(|p| p.get("summary"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        add_unique_schema_diff(schema_diff, "payload.summary missing".to_string());
    }
}

fn validate_blocker_payload_placement(
    schema_diff: &mut Vec<String>,
    obj: &serde_json::Map<String, Value>,
) {
    if obj.contains_key("blocker")
        || obj.contains_key("evidence")
        || obj.contains_key("required_action")
    {
        add_unique_schema_diff(
            schema_diff,
            "blocker fields must be inside payload: blocker/evidence/required_action".to_string(),
        );
    }
}

fn validate_blocker_payload_fields(
    schema_diff: &mut Vec<String>,
    payload: Option<&serde_json::Map<String, Value>>,
    msg_type: Option<&str>,
    msg_status: Option<&str>,
) {
    let is_blocker = msg_type
        .map(|v| v.eq_ignore_ascii_case("blocker"))
        .unwrap_or(false)
        || msg_status
            .map(|v| v.eq_ignore_ascii_case("blocked"))
            .unwrap_or(false);
    if is_blocker {
        for field in ["blocker", "evidence", "required_action"] {
            push_missing_string_payload_field(schema_diff, payload, field);
        }
    }
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
    let default_to = defaults.to_role;
    let default_type = defaults.msg_type;
    let default_status = defaults.status;
    let mut changed = false;
    // Always force `from` to the actual running role — never trust the model to
    // self-report its identity.  Models copy `"from": "planner"` from context
    // messages and emit it verbatim even when running as executor.
    let actual_from = default_from; // default_from is derived from role via MessageRoute
    if obj.get("from").and_then(|v| v.as_str()) != Some(actual_from) {
        if let Some(wrong) = obj.get("from").and_then(|v| v.as_str()) {
            if !wrong.eq_ignore_ascii_case(actual_from) {
                eprintln!(
                    "[{role}] auto_fill_message_fields: correcting `from` field `{wrong}` → `{actual_from}`"
                );
            }
        }
        obj.insert("from".to_string(), Value::String(actual_from.to_string()));
        changed = true;
    }
    changed |= ensure_object_string_field(obj, "to", default_to);
    changed |= ensure_object_string_field(obj, "type", default_type);
    changed |= ensure_object_string_field(obj, "status", default_status);
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

    if missing_predicted_next_actions(obj) {
        obj.insert(
            "predicted_next_actions".to_string(),
            example_predicted_next_actions(),
        );
        changed = true;
    }

    if let Some(payload) = obj.get_mut("payload").and_then(|v| v.as_object_mut()) {
        changed |= ensure_object_string_field(payload, "summary", "auto-filled message fields");
        changed |= ensure_object_string_field(
            payload,
            "expected_format",
            &expected_message_format(&from_val, &to_val, &type_val, &status_val),
        );
        if is_blocker {
            changed |= ensure_blocker_payload_fields(payload);
        }
    }

    changed |= ensure_object_string_field(
        obj,
        "observation",
        "Auto-filled missing message fields.",
    );
    changed |= ensure_object_string_field(
        obj,
        "rationale",
        "Repair invalid message schema to continue execution.",
    );
    changed
}

fn object_string_present(obj: &serde_json::Map<String, Value>, field: &str) -> bool {
    obj.get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some()
}

fn ensure_object_string_field(
    obj: &mut serde_json::Map<String, Value>,
    field: &str,
    value: &str,
) -> bool {
    if object_string_present(obj, field) {
        return false;
    }
    obj.insert(field.to_string(), Value::String(value.to_string()));
    true
}

fn missing_predicted_next_actions(obj: &serde_json::Map<String, Value>) -> bool {
    obj.get("predicted_next_actions")
        .and_then(|v| v.as_array())
        .filter(|items| (2..=3).contains(&items.len()))
        .is_none()
}

fn ensure_blocker_payload_fields(payload: &mut serde_json::Map<String, Value>) -> bool {
    let mut changed = false;
    for (field, value) in [
        ("blocker", "auto-filled blocker details"),
        ("evidence", "auto-filled blocker evidence"),
        ("required_action", "auto-filled required action"),
    ] {
        changed |= ensure_object_string_field(payload, field, value);
    }
    changed
}

/// Autofill missing provenance fields on **any** action to stop the schema-rejection loop.
///
/// Called after `auto_fill_message_fields` so message-specific logic runs first.
/// Only fills fields that are absent or empty; never overwrites present values.
/// Returns `true` if any field was added.
pub fn ensure_action_base_schema(action: &mut Value) -> bool {
    let Some(obj) = action.as_object_mut() else {
        return false;
    };
    let mut changed = false;

    // rationale — required, must be non-empty
    changed |= ensure_object_string_field(obj, "rationale", "Auto-filled to satisfy schema.");

    // predicted_next_actions — required array of 2-3 items
    if missing_predicted_next_actions(obj) {
        obj.insert(
            "predicted_next_actions".to_string(),
            example_predicted_next_actions(),
        );
        changed = true;
    }

    // intent — optional but must be non-empty when present; autofill if missing
    changed |= ensure_object_string_field(obj, "intent", "Auto-filled intent.");

    // task_id / objective_id — optional provenance; only inject when completely absent
    // (empty string is already a schema violation so we leave those for corrective feedback)
    if !obj.contains_key("task_id") {
        obj.insert(
            "task_id".to_string(),
            Value::String("unknown".to_string()),
        );
        changed = true;
    }
    if !obj.contains_key("objective_id") {
        obj.insert(
            "objective_id".to_string(),
            Value::String("unknown".to_string()),
        );
        changed = true;
    }

    changed
}
