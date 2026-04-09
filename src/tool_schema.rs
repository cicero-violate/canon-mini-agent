use anyhow::{anyhow, Result};
use jsonschema::error::{TypeKind, ValidationErrorKind};
use jsonschema::JSONSchema;
use schemars::{schema_for, JsonSchema};
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
