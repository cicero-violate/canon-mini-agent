use anyhow::{anyhow, Result};
use jsonschema::error::{TypeKind, ValidationErrorKind};
use jsonschema::JSONSchema;
use schemars::{schema_for, JsonSchema};
use schemars::schema::SchemaObject;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PredictedNextAction {
    pub action: PredictedActionName,
    #[schemars(length(min = 1))]
    pub intent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PredictedActionName {
    Message,
    ListDir,
    ReadFile,
    Objectives,
    ApplyPatch,
    RunCommand,
    Python,
    CargoTest,
    Plan,
    RustcHir,
    RustcMir,
    GraphCall,
    GraphCfg,
    GraphDataflow,
    GraphReachability,
}

pub fn predicted_action_name_list() -> Vec<String> {
    let schema = schema_for!(PredictedActionName);
    extract_enum_strings(&schema.schema).unwrap_or_default()
}

pub const TOOL_ACTION_NAMES: &[&str] = &[
    "message",
    "list_dir",
    "read_file",
    "objectives",
    "apply_patch",
    "run_command",
    "python",
    "cargo_test",
    "plan",
];

pub const ALL_TOOL_PROMPT_KINDS: &[&str] = &[
    "list_dir",
    "read_file",
    "objectives",
    "apply_patch",
    "run_command",
    "python",
    "cargo_test",
    "plan",
    "message",
];

fn extract_enum_strings(schema: &SchemaObject) -> Option<Vec<String>> {
    let enums = schema.enum_values.as_ref()?;
    let mut out = Vec::with_capacity(enums.len());
    for value in enums {
        if let Some(s) = value.as_str() {
            out.push(s.to_string());
        }
    }
    Some(out)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionBase {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<String>,
    #[schemars(length(min = 1))]
    pub rationale: String,
    pub predicted_next_actions: Vec<PredictedNextAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessagePayload {
    #[schemars(length(min = 1))]
    pub summary: String,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanOp {
    CreateTask,
    UpdateTask,
    DeleteTask,
    AddEdge,
    RemoveEdge,
    SetStatus,
    ReplacePlan,
    SortedView,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObjectivesOp {
    Read,
    CreateObjective,
    UpdateObjective,
    DeleteObjective,
    SetStatus,
    ReplaceObjectives,
    SortedView,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ToolAction {
    Message {
        #[serde(flatten)]
        base: ActionBase,
        from: String,
        to: String,
        #[serde(rename = "type")]
        msg_type: String,
        status: String,
        payload: MessagePayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        severity: Option<String>,
    },
    ListDir {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    ReadFile {
        #[serde(flatten)]
        base: ActionBase,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u64>,
    },
    Objectives {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op: Option<ObjectivesOp>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        objective_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        objective: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updates: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        objectives: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        include_done: Option<bool>,
    },
    ApplyPatch {
        #[serde(flatten)]
        base: ActionBase,
        patch: String,
    },
    RunCommand {
        #[serde(flatten)]
        base: ActionBase,
        cmd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    Python {
        #[serde(flatten)]
        base: ActionBase,
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    CargoTest {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        test: Option<String>,
    },
    Plan {
        #[serde(flatten)]
        base: ActionBase,
        op: PlanOp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updates: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    RustcHir {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        mode: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra: Option<String>,
    },
    RustcMir {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        mode: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra: Option<String>,
    },
    GraphCall {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        out_dir: Option<String>,
    },
    GraphCfg {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        out_dir: Option<String>,
    },
    GraphDataflow {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tlog: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        out_dir: Option<String>,
    },
    GraphReachability {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tlog: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        out_dir: Option<String>,
    },
}

pub fn tool_protocol_schema_json() -> String {
    let schema = schema_for!(ToolAction);
    serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string())
}

pub fn tool_protocol_schema_split_text() -> String {
    let schema = schema_for!(ToolAction);
    let value = serde_json::to_value(&schema).unwrap_or_else(|_| Value::Object(Default::default()));
    let mut out = String::new();
    out.push_str(
        "Each action has its own schema; choose the schema that matches the `action` field.\n",
    );
    out.push_str(
        "Common fields appear in every action: `rationale` (non-empty), `predicted_next_actions` (2-3 items), and optional `observation`.\n\n",
    );

    let actions = [
        (
            "message",
            "send inter-agent protocol message",
            Some(
                "Examples:\n  {\"action\":\"message\",\"from\":\"executor\",\"to\":\"verifier\",\"type\":\"handoff\",\"status\":\"complete\",\"observation\":\"Summarize what happened.\",\"rationale\":\"Execution work is complete and the verifier now has enough evidence to judge it.\",\"payload\":{\"summary\":\"brief evidence summary\",\"artifacts\":[\"path/to/file.rs\"]}}\n  {\"action\":\"message\",\"from\":\"executor\",\"to\":\"planner\",\"type\":\"blocker\",\"status\":\"blocked\",\"observation\":\"Describe the blocker.\",\"rationale\":\"Explain why progress is impossible.\",\"payload\":{\"summary\":\"Short blocker summary\",\"blocker\":\"Root cause\",\"evidence\":\"Concrete error text\",\"required_action\":\"What must be done to unblock\",\"severity\":\"error\"}}\nAllowed roles: executor|planner|verifier|diagnostics|solo. Allowed types: handoff|result|verification|failure|blocker|plan|diagnostics. Allowed status: complete|in_progress|failed|verified|ready|blocked.\n⚠ message with status=complete is REJECTED if build or tests fail — fix all errors first.",
            ),
        ),
        (
            "list_dir",
            "inspect directory contents",
            Some("Example:\n  {\"action\":\"list_dir\",\"path\":\".\",\"rationale\":\"Inspect the workspace before making assumptions.\"}"),
        ),
        (
            "read_file",
            "read a file; output is line-numbered",
            Some(
                "Examples:\n  {\"action\":\"read_file\",\"path\":\"src/app.rs\",\"rationale\":\"Read the file before editing it.\"}\n  {\"action\":\"read_file\",\"path\":\"src/app.rs\",\"line\":120,\"rationale\":\"Read the relevant section before editing it.\"}\nWith \"line\":N the output starts at line N and shows up to 1000 lines.\n⚠ Always read a file before patching it. Never patch from memory.\n⚠ Paths may be relative to WORKSPACE or absolute under WORKSPACE.\n⚠ read_file output is prefixed with line numbers (\"42: code here\"). Strip the \"N: \" prefix when writing patch lines.\nWRONG:  -42: fn old() {}   RIGHT:  -fn old() {}",
            ),
        ),
        (
            "objectives",
            "read/update objectives in PLANS/OBJECTIVES.json",
            Some(
                "Examples:\n  {\"action\":\"objectives\",\"op\":\"read\",\"rationale\":\"Load only non-completed objectives for planning/verification.\"}\n  {\"action\":\"objectives\",\"op\":\"read\",\"include_done\":true,\"rationale\":\"Load all objectives, including completed.\"}\n  {\"action\":\"objectives\",\"op\":\"create_objective\",\"objective\":{\"id\":\"obj_new\",\"title\":\"New objective\",\"status\":\"active\",\"scope\":\"...\",\"authority_files\":[\"src/foo.rs\"],\"category\":\"quality\",\"level\":\"low\",\"description\":\"...\",\"requirement\":[],\"verification\":[],\"success_criteria\":[]},\"rationale\":\"Record a new objective.\"}\n  {\"action\":\"objectives\",\"op\":\"set_status\",\"objective_id\":\"obj_new\",\"status\":\"done\",\"rationale\":\"Mark objective complete.\"}\n  {\"action\":\"objectives\",\"op\":\"update_objective\",\"objective_id\":\"obj_new\",\"updates\":{\"scope\":\"updated scope\"},\"rationale\":\"Update objective fields.\"}\n  {\"action\":\"objectives\",\"op\":\"delete_objective\",\"objective_id\":\"obj_new\",\"rationale\":\"Remove obsolete objective.\"}\n  {\"action\":\"objectives\",\"op\":\"replace_objectives\",\"objectives\":[],\"rationale\":\"Replace objectives list.\"}\n  {\"action\":\"objectives\",\"op\":\"sorted_view\",\"rationale\":\"View objectives sorted by status.\"}",
            ),
        ),
        (
            "apply_patch",
            "create or update files using unified patch syntax",
            Some(
                "Examples:\n  {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Add File: path/to/new.rs\\n+line one\\n+line two\\n*** End Patch\",\"rationale\":\"Apply the concrete code change after reading the target context.\"}\n  {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Update File: src/lib.rs\\n@@\\n fn before_before() {}\\n fn before() {}\\n fn target() {\\n-    old_body();\\n+    new_body();\\n }\\n fn after() {}\\n*** End Patch\",\"rationale\":\"Update the file using exact surrounding context from the read.\"}\n  {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Delete File: PLANS/executor-b.json\\n*** Add File: PLANS/executor-b.json\\n+# new content\\n+line two\\n*** End Patch\",\"rationale\":\"Full-file replacement is safer than a giant hunk with many - lines.\"}\nRules:\n- Every @@ hunk must have AT LEAST 3 unchanged context lines around the edit.\n- Never use @@ with only 1 context line.\n- ALL - lines must be copied character-for-character from read_file output (minus the \"N: \" prefix).\n- If replacing more than ~10 lines, use *** Delete File + *** Add File instead of a large @@ hunk.\n- NEVER use absolute paths inside the patch string.",
            ),
        ),
        (
            "run_command",
            "run shell commands for discovery or verification",
            Some(
                "Examples:\n  {\"action\":\"run_command\",\"cmd\":\"cargo check -p canon-mini-agent\",\"cwd\":\"/workspace/ai_sandbox/canon-mini-agent\",\"rationale\":\"Validate the target crate after a change.\"}\n  {\"action\":\"run_command\",\"cmd\":\"rg -n 'fn foo' src\",\"cwd\":\"/workspace/ai_sandbox/canon-mini-agent\",\"rationale\":\"Search the codebase for the relevant symbol before editing.\"}\n⚠ cwd may be relative to WORKSPACE or absolute under WORKSPACE.",
            ),
        ),
        (
            "python",
            "run Python analysis inside the workspace",
            Some(
                "Example:\n  {\"action\":\"python\",\"code\":\"from pathlib import Path\\nprint(len(list(Path('src').glob('**/*.rs'))))\",\"cwd\":\"/workspace/ai_sandbox/canon-mini-agent\",\"rationale\":\"Use Python for structured workspace analysis.\"}\n⚠ cwd may be relative to WORKSPACE or absolute under WORKSPACE.",
            ),
        ),
        (
            "cargo_test",
            "run a targeted cargo test (harness-style)",
            Some(
                "Example:\n  {\"action\":\"cargo_test\",\"crate\":\"canon-runtime\",\"test\":\"some_test_name\",\"rationale\":\"Run the exact failing test using the harness-style command.\"}",
            ),
        ),
        (
            "plan",
            "create/update/delete tasks and DAG edges in PLAN.json",
            Some(
                "Examples:\n  {\"action\":\"plan\",\"op\":\"set_status\",\"task_id\":\"T1\",\"status\":\"in_progress\",\"rationale\":\"Update PLAN.json via the plan tool while running solo.\"}\n  {\"action\":\"plan\",\"op\":\"sorted_view\",\"rationale\":\"View the current plan in DAG order (read-only).\"}",
            ),
        ),
        ("rustc_hir", "emit HIR for analysis", None),
        ("rustc_mir", "emit MIR for analysis", None),
        ("graph_call", "emit call graph CSVs", None),
        ("graph_cfg", "emit CFG CSVs", None),
        ("graph_dataflow", "emit dataflow reports", None),
        ("graph_reachability", "emit reachability reports", None),
    ];

    for (action, desc, notes) in actions {
        let schema = find_action_schema(&value, action)
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_else(|| "{}".to_string());
        out.push_str(&format!("Action: `{action}` — {desc}\n```json\n{schema}\n```\n\n"));
        if let Some(notes) = notes {
            out.push_str(notes);
            out.push_str("\n\n");
        }
    }

    if let Some(defs) = value.get("definitions") {
        if let Ok(defs_json) = serde_json::to_string_pretty(defs) {
            out.push_str("Shared definitions (referenced via `$ref`):\n```json\n");
            out.push_str(&defs_json);
            out.push_str("\n```\n");
        }
    }

    out
}

pub(crate) fn validate_tool_action(action: &Value) -> Result<()> {
    static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
    let compiled = SCHEMA.get_or_init(|| {
        let schema = schema_for!(ToolAction);
        let value = serde_json::to_value(&schema).expect("tool schema to value");
        JSONSchema::compile(&value).expect("compile tool schema")
    });
    if let Err(errors) = compiled.validate(action) {
        let mut details = Vec::new();
        for err in errors.take(5) {
            details.push(err.to_string());
        }
        let suffix = if details.is_empty() { "" } else { ": " };
        return Err(anyhow!("action schema invalid{suffix}{}", details.join("; ")));
    }
    // Manual guards not expressible in schemars 0.8
    if let Some(rationale) = action.get("rationale").and_then(|v| v.as_str()) {
        if rationale.trim().is_empty() {
            return Err(anyhow!("action missing non-empty 'rationale'"));
        }
    } else {
        return Err(anyhow!("action missing non-empty 'rationale'"));
    }
    let predicted = action
        .get("predicted_next_actions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("action missing 'predicted_next_actions'"))?;
    if !(2..=3).contains(&predicted.len()) {
        return Err(anyhow!("predicted_next_actions must contain 2-3 entries"));
    }
    for (idx, item) in predicted.iter().enumerate() {
        let intent = item
            .get("intent")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if intent.is_empty() {
            return Err(anyhow!("predicted_next_actions[{idx}] missing non-empty 'intent'"));
        }
    }
    Ok(())
}

pub(crate) fn schema_diff_messages(action: &Value) -> Vec<String> {
    static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
    let compiled = SCHEMA.get_or_init(|| {
        let schema = schema_for!(ToolAction);
        let value = serde_json::to_value(&schema).expect("tool schema to value");
        JSONSchema::compile(&value).expect("compile tool schema")
    });
    let mut diffs = Vec::new();
    let Err(errors) = compiled.validate(action) else {
        return diffs;
    };
    for err in errors.take(10) {
        diffs.push(map_schema_error(&err, action));
    }
    diffs
}

pub(crate) fn action_schema_json(action: &str) -> Option<String> {
    let schema = schema_for!(ToolAction);
    let value = serde_json::to_value(&schema).ok()?;
    let schema = find_action_schema(&value, action)?;
    serde_json::to_string_pretty(schema).ok()
}

fn map_schema_error(err: &jsonschema::error::ValidationError, action: &Value) -> String {
    let path = path_from_error(err);
    match &err.kind {
        ValidationErrorKind::Required { property } => {
            let property = property.as_str().unwrap_or("unknown");
            if path.is_empty() {
                format!("missing field: {property}")
            } else {
                format!("{path}.{property} missing")
            }
        }
        ValidationErrorKind::Type { kind } => {
            let field = if path.is_empty() { "action".to_string() } else { path };
            format!(
                "field type mismatch: {field} (expected {})",
                type_kind_label(kind)
            )
        }
        ValidationErrorKind::MinLength { .. } => {
            let field = if path.is_empty() { "action".to_string() } else { path };
            format!("missing field: {field}")
        }
        ValidationErrorKind::MinItems { .. } | ValidationErrorKind::MaxItems { .. } => {
            "predicted_next_actions must contain 2-3 entries".to_string()
        }
        ValidationErrorKind::Enum { .. } => {
            if path == "op" {
                format!("unknown plan op: {}", stringify_instance(&err.instance))
            } else if path == "action" || path.ends_with(".action") {
                format!("unsupported action: {}", stringify_instance(&err.instance))
            } else {
                format!("enum mismatch: {path}")
            }
        }
        ValidationErrorKind::OneOfNotValid | ValidationErrorKind::AnyOf => {
            let action_val = action.get("action").and_then(|v| v.as_str());
            if let Some(action_val) = action_val {
                if is_known_action(action_val) {
                    if let Some(missing) = first_missing_field_for_action(action, action_val) {
                        return missing;
                    }
                    return format!("action schema mismatch: {action_val}");
                }
                return format!("unsupported action: {action_val}");
            }
            "unsupported action: missing or unknown action".to_string()
        }
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            format!("unexpected fields: {}", unexpected.join(", "))
        }
        other => format!("schema violation: {other:?}"),
    }
}

fn find_action_schema<'a>(value: &'a Value, action: &str) -> Option<&'a Value> {
    fn matches_action(value: &Value, action: &str) -> bool {
        let action_prop = value.get("properties").and_then(|p| p.get("action"));
        let const_match = action_prop
            .and_then(|a| a.get("const"))
            .and_then(|c| c.as_str())
            == Some(action);
        let enum_match = action_prop
            .and_then(|a| a.get("enum"))
            .and_then(|e| e.as_array())
            .map(|arr| arr.iter().any(|v| v.as_str() == Some(action)))
            .unwrap_or(false);
        const_match || enum_match
    }

    if matches_action(value, action) {
        return Some(value);
    }

    if let Some(arr) = value.get("oneOf").and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(found) = find_action_schema(item, action) {
                return Some(found);
            }
        }
    }
    if let Some(arr) = value.get("anyOf").and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(found) = find_action_schema(item, action) {
                return Some(found);
            }
        }
    }
    match value {
        Value::Array(arr) => {
            for item in arr {
                if let Some(found) = find_action_schema(item, action) {
                    return Some(found);
                }
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                if let Some(found) = find_action_schema(item, action) {
                    return Some(found);
                }
            }
        }
        _ => {}
    }
    None
}

fn path_from_error(err: &jsonschema::error::ValidationError) -> String {
    let path = err.instance_path.to_string();
    if path.is_empty() {
        return String::new();
    }
    path.trim_start_matches('/')
        .split('/')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(".")
}

fn stringify_instance(value: &Cow<'_, Value>) -> String {
    match value.as_ref() {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn type_kind_label(kind: &TypeKind) -> &'static str {
    let debug = format!("{kind:?}").to_ascii_lowercase();
    if debug.contains("array") {
        "array"
    } else if debug.contains("boolean") {
        "boolean"
    } else if debug.contains("integer") {
        "integer"
    } else if debug.contains("null") {
        "null"
    } else if debug.contains("number") {
        "number"
    } else if debug.contains("object") {
        "object"
    } else if debug.contains("string") {
        "string"
    } else {
        "multiple"
    }
}

fn is_known_action(action: &str) -> bool {
    matches!(
        action,
        "message"
            | "list_dir"
            | "read_file"
            | "objectives"
            | "apply_patch"
            | "run_command"
            | "python"
            | "cargo_test"
            | "plan"
            | "rustc_hir"
            | "rustc_mir"
            | "graph_call"
            | "graph_cfg"
            | "graph_dataflow"
            | "graph_reachability"
    )
}

fn first_missing_field_for_action(action: &Value, action_name: &str) -> Option<String> {
    let missing_field = |field: &str| {
        action.get(field).is_none().then(|| format!("missing field: {field}"))
    };
    if action.get("predicted_next_actions").is_none() {
        return Some("missing field: predicted_next_actions".to_string());
    }
    match action_name {
        "message" => {
            for field in ["from", "to", "type", "status", "payload"] {
                if let Some(missing) = missing_field(field) {
                    return Some(missing);
                }
            }
            None
        }
        "list_dir" => missing_field("path"),
        "read_file" => missing_field("path"),
        "objectives" => {
            let op = action.get("op").and_then(|v| v.as_str()).unwrap_or("read");
            let id_missing = || {
                if action.get("objective_id").is_none() && action.get("id").is_none() {
                    Some("missing field: objective_id".to_string())
                } else {
                    None
                }
            };
            match op {
                "read" | "sorted_view" => None,
                "create_objective" => missing_field("objective"),
                "update_objective" => id_missing().or_else(|| missing_field("updates")),
                "delete_objective" => id_missing(),
                "set_status" => id_missing().or_else(|| missing_field("status")),
                "replace_objectives" => missing_field("objectives"),
                _ => None,
            }
        }
        "apply_patch" => missing_field("patch"),
        "run_command" => missing_field("cmd"),
        "python" => missing_field("code"),
        "cargo_test" => missing_field("crate"),
        "plan" => missing_field("op"),
        "rustc_hir" | "rustc_mir" => missing_field("crate"),
        "graph_call" | "graph_cfg" | "graph_dataflow" | "graph_reachability" => {
            missing_field("crate")
        }
        _ => None,
    }
}
