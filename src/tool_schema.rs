use anyhow::{anyhow, Result};
use jsonschema::error::{TypeKind, ValidationErrorKind};
use jsonschema::JSONSchema;
use schemars::schema::SchemaObject;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PredictedNextAction {
    pub action: PredictedActionName,
    #[schemars(length(min = 1, max = 80))]
    pub intent: String,
}

const ACTION_OBSERVATION_MAX_LEN: usize = 400;
const ACTION_RATIONALE_MAX_LEN: usize = 300;
const PREDICTED_INTENT_MAX_LEN: usize = 80;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PredictedActionName {
    Message,
    ListDir,
    ReadFile,
    SymbolsIndex,
    SymbolsRenameCandidates,
    SymbolsPrepareRename,
    RenameSymbol,
    Objectives,
    Issue,
    ApplyPatch,
    RunCommand,
    Python,
    CargoTest,
    CargoFmt,
    CargoClippy,
    Plan,
    RustcHir,
    RustcMir,
    GraphCall,
    GraphCfg,
    GraphDataflow,
    GraphReachability,
    StageGraph,
    SemanticMap,
    SymbolWindow,
    SymbolRefs,
    SymbolPath,
    ExecutionPath,
    SymbolNeighborhood,
    Batch,
    Lessons,
    Violation,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenamePair {
    /// Canonical-ish symbol path for the old symbol (module-relative, e.g. `tools::my_fn`).
    /// Crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted.
    #[schemars(length(min = 1))]
    pub old: String,
    /// Canonical-ish symbol path for the new symbol. Only the last path segment is used as
    /// the identifier replacement (e.g. `tools::new_name` → `new_name`).
    #[schemars(length(min = 1))]
    pub new: String,
}

pub fn predicted_action_name_list() -> Vec<String> {
    let schema = schema_for!(PredictedActionName);
    extract_enum_strings(&schema.schema).unwrap_or_default()
}

pub fn cargo_test_action_example() -> &'static str {
    "Example:\n  {\"action\":\"cargo_test\",\"crate\":\"canon-mini-agent\",\"test\":\"some_test_name\",\"rationale\":\"Run the exact failing test using the harness-style command.\"}"
}

fn compact_example_json(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

pub fn plan_set_task_status_action_example(task_id: &str, status: &str, rationale: &str) -> String {
    compact_example_json(json!({
        "action": "plan",
        "op": "set_task_status",
        "task_id": task_id,
        "status": status,
        "rationale": rationale
    }))
}

pub fn plan_set_plan_status_action_example(status: &str, rationale: &str) -> String {
    compact_example_json(json!({
        "action": "plan",
        "op": "set_plan_status",
        "status": status,
        "rationale": rationale
    }))
}

pub fn plan_create_task_action_example(task_id: &str, rationale: &str) -> String {
    compact_example_json(json!({
        "action": "plan",
        "op": "create_task",
        "task": {
            "id": task_id,
            "title": "Add the missing dependency edge",
            "status": "ready",
            "priority": 1,
            "description": "Record the next planner follow-up explicitly in PLAN.json."
        },
        "rationale": rationale
    }))
}

pub fn plan_add_edge_action_example(from: &str, to: &str, rationale: &str) -> String {
    compact_example_json(json!({
        "action": "plan",
        "op": "add_edge",
        "from": from,
        "to": to,
        "rationale": rationale
    }))
}

pub fn plan_remove_edge_action_example(from: &str, to: &str, rationale: &str) -> String {
    compact_example_json(json!({
        "action": "plan",
        "op": "remove_edge",
        "from": from,
        "to": to,
        "rationale": rationale
    }))
}

pub fn plan_update_bundle_action_example(rationale: &str) -> String {
    compact_example_json(json!({
        "action": "plan",
        "op": "update",
        "updates": {
            "status": "in_progress",
            "ready_window": ["T_restore_missing_diagnostics_artifacts"],
            "tasks": [
                {
                    "id": "T_restore_missing_diagnostics_artifacts",
                    "title": "Restore diagnostics input artifacts",
                    "status": "ready",
                    "priority": 1
                }
            ]
        },
        "rationale": rationale
    }))
}

pub fn plan_sorted_view_action_example(rationale: &str) -> String {
    compact_example_json(json!({
        "action": "plan",
        "op": "sorted_view",
        "rationale": rationale
    }))
}

fn schema_enum_values<T: JsonSchema>() -> Vec<String> {
    let schema = schema_for!(T);
    extract_enum_strings(&schema.schema).unwrap_or_default()
}

pub fn plan_action_examples_block() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        let ops = schema_enum_values::<PlanOp>();
        let mut lines = vec![format!(
            "Allowed `plan.op` values (schema-derived): {}",
            ops.join(", ")
        )];
        lines.push("Examples:".to_string());
        if ops.iter().any(|op| op == "create_task") {
            lines.push(format!(
                "  {}",
                plan_create_task_action_example(
                    "T_add_missing_dependency_edge",
                    "Seed a new planner task in PLAN.json."
                )
            ));
        }
        if ops.iter().any(|op| op == "add_edge") {
            lines.push(format!(
                "  {}",
                plan_add_edge_action_example(
                    "T_read_action_input",
                    "T_emit_prediction_from_stdin",
                    "Add an explicit DAG edge between two existing tasks."
                )
            ));
        }
        if ops.iter().any(|op| op == "remove_edge") {
            lines.push(format!(
                "  {}",
                plan_remove_edge_action_example(
                    "T_old_dependency",
                    "T_blocked_task",
                    "Remove an obsolete DAG edge when sequencing changed."
                )
            ));
        }
        if ops.iter().any(|op| op == "set_task_status") {
            lines.push(format!(
                "  {}",
                plan_set_task_status_action_example(
                    "T1",
                    "in_progress",
                    "Update a single task status in PLAN.json."
                )
            ));
        }
        if ops.iter().any(|op| op == "set_plan_status") {
            lines.push(format!(
                "  {}",
                plan_set_plan_status_action_example(
                    "in_progress",
                    "Update top-level PLAN.json status."
                )
            ));
        }
        if ops.iter().any(|op| op == "update") {
            lines.push(format!(
                "  {}",
                plan_update_bundle_action_example(
                    "Apply a bundled PLAN.json update when ready_window, tasks, or status must change together."
                )
            ));
        }
        if ops.iter().any(|op| op == "sorted_view") {
            lines.push(format!(
                "  {}",
                plan_sorted_view_action_example("View the current plan in DAG order (read-only).")
            ));
        }
        lines.push(
            "Use `add_edge` / `remove_edge` for DAG edges — never invent `create_edge` / `delete_edge`."
                .to_string(),
        );
        lines.join("\n")
    })
    .as_str()
}

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
    #[schemars(length(max = 400))]
    pub observation: Option<String>,
    #[schemars(length(min = 1, max = 300))]
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub question: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub objective_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub intent: Option<String>,
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
    SetPlanStatus,
    SetTaskStatus,
    ReplacePlan,
    Update,
    SortedView,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueOp {
    Read,
    Create,
    Update,
    Delete,
    SetStatus,
    Upsert,
    Resolve,
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
    Replace,
    SortedView,
}

/// The subset of actions allowed inside a `batch`. All are non-mutating.
/// `plan`, `objectives`, and `issue` are included but only read-only ops
/// are accepted at runtime (sorted_view / read).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BatchableActionName {
    ReadFile,
    ListDir,
    SemanticMap,
    SymbolWindow,
    SymbolRefs,
    SymbolPath,
    ExecutionPath,
    SymbolNeighborhood,
    SymbolsIndex,
    SymbolsRenameCandidates,
    SymbolsPrepareRename,
    StageGraph,
    RustcHir,
    RustcMir,
    GraphCall,
    GraphCfg,
    GraphDataflow,
    GraphReachability,
    /// Only `sorted_view` op is accepted at runtime.
    Plan,
    /// Only `read` and `sorted_view` ops are accepted at runtime.
    Objectives,
    /// Only `read` op is accepted at runtime.
    Issue,
}

/// A single sub-action inside a `batch`. Carries the same fields as the
/// corresponding top-level action, but without `rationale`,
/// `predicted_next_actions`, and `observation` (those belong on the outer
/// `batch` envelope).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BatchItem {
    pub action: BatchableActionName,
    #[serde(flatten)]
    pub params: std::collections::BTreeMap<String, serde_json::Value>,
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
    SymbolsIndex {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        out: Option<String>,
    },
    SymbolsRenameCandidates {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbols_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        out: Option<String>,
    },
    SymbolsPrepareRename {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        candidates_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        out: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<u64>,
    },
    RenameSymbol {
        #[serde(flatten)]
        base: ActionBase,
        /// Optional crate name for loading `state/rustc/<crate>/graph.json` (defaults to `canon_mini_agent`).
        /// Hyphens are normalized to underscores.
        #[serde(rename = "crate", default, skip_serializing_if = "Option::is_none")]
        crate_name: Option<String>,
        /// Shorthand single rename pair. Prefer `renames` for bulk.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_symbol: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_symbol: Option<String>,
        /// Bulk renames. If present and non-empty, takes precedence over `old_symbol`/`new_symbol`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        renames: Option<Vec<RenamePair>>,
    },
    Issue {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op: Option<IssueOp>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence_receipts: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issue_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issue: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updates: Option<serde_json::Value>,
    },
    Objectives {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op: Option<ObjectivesOp>,
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
    CargoFmt {
        #[serde(flatten)]
        base: ActionBase,
        /// When true, runs `cargo fmt` (may modify files). When false (default), runs `cargo fmt --check`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        fix: bool,
    },
    CargoClippy {
        #[serde(flatten)]
        base: ActionBase,
        /// Optional crate selector. When set, runs `cargo clippy -p <crate> -D warnings`.
        /// When omitted, runs `cargo clippy -D warnings` for the workspace.
        #[serde(rename = "crate", default, skip_serializing_if = "Option::is_none")]
        crate_name: Option<String>,
    },
    Plan {
        #[serde(flatten)]
        base: ActionBase,
        op: PlanOp,
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
        /// Optional symbol path to focus the output on one item (e.g. "tools::handle_objectives_action").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra: Option<String>,
    },
    RustcMir {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        mode: String,
        /// Optional symbol path to focus the output on one item (e.g. "tools::handle_objectives_action").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
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
    /// Emit a synthetic OODA-style stage graph artifact (independent of rustc call graph).
    StageGraph {
        #[serde(flatten)]
        base: ActionBase,
        /// Output path. Defaults to `agent_state/orchestrator/stage_graph.json`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        out: Option<String>,
    },
    /// Triple-style semantic graph view for a crate (backed by rustc graph.json).
    /// Emits one `(from, relation, to)` line per edge.
    SemanticMap {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        /// Optional symbol-path prefix to restrict output (e.g. "canon_mini_agent::tools").
        /// Keeps triples whose source or target matches the prefix.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
        /// Retained for backward compatibility; ignored in triple mode.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        expand_bodies: bool,
    },
    /// Extract the full definition body of a symbol using byte-precise def span.
    SymbolWindow {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        /// Fully-qualified symbol path (e.g. "canon_mini_agent::tools::execute_logged_action").
        symbol: String,
    },
    /// List all reference sites (ident spans) for a symbol across the crate.
    /// Set `expand_bodies` to true to also show the enclosing function/struct/trait body at each site (like symbol_window).
    SymbolRefs {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        symbol: String,
        /// When true, includes the full enclosing symbol body at each reference site.
        /// Defaults to false (file:line:col only).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        expand_bodies: bool,
    },
    /// BFS shortest call-graph path between two symbols.
    /// Set `expand_bodies` to true to inline the source body of each hop along the path.
    SymbolPath {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        from: String,
        to: String,
        /// When true, inlines the full source body of each symbol along the call path.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        expand_bodies: bool,
    },
    /// BFS shortest path across semantic nodes, CFG blocks, and bridge edges.
    ExecutionPath {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        from: String,
        to: String,
        /// When true, inlines the owner symbol body for semantic hops and CFG-owned blocks.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        expand_bodies: bool,
    },
    /// Immediate callers and callees of a symbol in the call graph.
    /// Set `expand_bodies` to true to inline the source body of each caller and callee.
    SymbolNeighborhood {
        #[serde(flatten)]
        base: ActionBase,
        #[serde(rename = "crate")]
        crate_name: String,
        symbol: String,
        /// When true, inlines the full source body of each caller and callee.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        expand_bodies: bool,
    },
    /// Execute multiple non-mutating actions in a single turn.
    ///
    /// All items must be read-only actions. Mutating actions (`apply_patch`,
    /// `rename_symbol`, `message`, `run_command`, `python`, `cargo_test`) are
    /// rejected. For `plan`, only `sorted_view` is accepted; for `objectives`,
    /// only `read`/`sorted_view`; for `issue`, only `read`.
    ///
    /// Max 8 items. Results are returned as labeled sections in declaration order.
    Batch {
        #[serde(flatten)]
        base: ActionBase,
        /// Sub-actions to execute. Each item carries an `action` field (from
        /// `BatchableActionName`) plus that action's normal parameters.
        /// Omit `rationale`, `predicted_next_actions`, and `observation` on
        /// items — those fields belong on the outer `batch` envelope.
        actions: Vec<BatchItem>,
    },
    /// Review and promote detected action patterns into the lessons artifact.
    ///
    /// Ops: read_candidates | promote | reject | encode | read | write
    Lessons {
        #[serde(flatten)]
        base: ActionBase,
        /// Which operation to perform. Defaults to `read_candidates` if omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op: Option<String>,
        /// For `promote`/`reject`: the candidate id to act on, or `"all"` for bulk promote.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        candidate_id: Option<String>,
        /// For `encode`: the exact entry text string from lessons.json to mark as encoded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_text: Option<String>,
        /// For `write`: a full LessonsArtifact object (summary, failures, fixes, required_actions).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lessons: Option<serde_json::Value>,
    },
    /// Review and manage dynamically discovered system invariants.
    ///
    /// Ops: read | promote | enforce | collapse
    Invariants {
        #[serde(flatten)]
        base: ActionBase,
        /// Which operation to perform. Defaults to `read` if omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op: Option<String>,
        /// For `promote` / `enforce` / `collapse`: the invariant id to act on.
        /// `promote` also accepts `"all"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Manage VIOLATIONS.json — add, update, resolve, or replace violation entries.
    ///
    /// Ops: read | upsert | resolve | set_status | replace
    Violation {
        #[serde(flatten)]
        base: ActionBase,
        /// Which operation to perform. Defaults to `read` if omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence_receipts: Option<Vec<String>>,
        /// For `upsert`: the full Violation object to add or replace (matched by id).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        violation: Option<serde_json::Value>,
        /// For `resolve`: the violation id to remove.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        violation_id: Option<String>,
        /// For `set_status`: the new report-level status string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// For `set_status`: optional updated summary string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// For `replace`: a full ViolationsReport object (status, summary, violations).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report: Option<serde_json::Value>,
    },
}

fn build_tool_actions_list() -> Vec<(&'static str, &'static str, Option<&'static str>)> {
    vec![
        (
            "message",
            "send inter-agent protocol message",
            Some(
                "Examples:\n  {\"action\":\"message\",\"from\":\"executor\",\"to\":\"planner\",\"type\":\"handoff\",\"status\":\"complete\",\"observation\":\"Summarize what happened.\",\"rationale\":\"Execution work is complete and planner now has enough evidence to schedule next work.\",\"payload\":{\"summary\":\"brief evidence summary\",\"artifacts\":[\"path/to/file.rs\"]}}\n  {\"action\":\"message\",\"from\":\"executor\",\"to\":\"planner\",\"type\":\"blocker\",\"status\":\"blocked\",\"observation\":\"Describe the blocker.\",\"rationale\":\"Explain why progress is impossible.\",\"payload\":{\"summary\":\"Short blocker summary\",\"blocker\":\"Root cause\",\"evidence\":\"Concrete error text\",\"required_action\":\"What must be done to unblock\",\"severity\":\"error\"}}\nAllowed roles: executor|planner. Allowed types: handoff|result|verification|failure|blocker|plan|diagnostics. Allowed status: complete|in_progress|failed|verified|ready|blocked.\n⚠ message with status=complete is REJECTED if build or tests fail — fix all errors first.",
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
            "symbols_index",
            "build deterministic symbols index JSON from Rust sources",
            Some(
                "Example:\n  {\"action\":\"symbols_index\",\"path\":\"src\",\"out\":\"state/symbols.json\",\"rationale\":\"Build a deterministic unique symbol catalog for planning and rename work.\",\"predicted_next_actions\":[{\"action\":\"read_file\",\"intent\":\"Inspect generated symbols.json and choose target symbols.\"},{\"action\":\"rename_symbol\",\"intent\":\"Apply a precise rename for one selected symbol.\"}]}\nNotes:\n- `path` defaults to workspace root.\n- `out` defaults to `state/symbols.json`.",
            ),
        ),
        (
            "symbols_rename_candidates",
            "derive deterministic rename candidates from symbols.json using naming heuristics",
            Some(
                "Example:\n  {\"action\":\"symbols_rename_candidates\",\"symbols_path\":\"state/symbols.json\",\"out\":\"state/rename_candidates.json\",\"rationale\":\"Surface high-value rename candidates before mutating code.\",\"predicted_next_actions\":[{\"action\":\"read_file\",\"intent\":\"Inspect rename candidates and choose one.\"},{\"action\":\"rename_symbol\",\"intent\":\"Apply a precise rename for the selected candidate.\"}]}\nNotes:\n- `symbols_path` defaults to `state/symbols.json`.\n- `out` defaults to `state/rename_candidates.json`.",
            ),
        ),
        (
            "symbols_prepare_rename",
            "pick a rename candidate and emit a ready-to-run rename_symbol action JSON skeleton",
            Some(
                "Example:\n  {\"action\":\"symbols_prepare_rename\",\"candidates_path\":\"state/rename_candidates.json\",\"index\":0,\"out\":\"state/next_rename_action.json\",\"rationale\":\"Pick the top candidate and prepare a deterministic rename action payload.\",\"predicted_next_actions\":[{\"action\":\"read_file\",\"intent\":\"Inspect prepared rename action JSON for correctness.\"},{\"action\":\"rename_symbol\",\"intent\":\"Execute the prepared rename action.\"}]}\nNotes:\n- `candidates_path` defaults to `state/rename_candidates.json`.\n- `index` defaults to 0.\n- `out` defaults to `state/next_rename_action.json`.",
            ),
        ),
        (
            "rename_symbol",
            "rename Rust symbols using rustc graph spans (crate-wide; supports bulk); auto cargo-check with git rollback on failure",
            Some(
                "Example (single):\n  {\"action\":\"rename_symbol\",\"old_symbol\":\"tools::handle_plan_action\",\"new_symbol\":\"tools::handle_master_plan_action\",\"question\":\"Is this the exact symbol to rename across the crate?\",\"rationale\":\"Use span-backed rename so all references update consistently.\",\"predicted_next_actions\":[{\"action\":\"cargo_test\",\"intent\":\"Run focused tests covering the renamed symbol.\"}]}\nExample (bulk):\n  {\"action\":\"rename_symbol\",\"renames\":[{\"old\":\"constants::EndpointSpec\",\"new\":\"constants::EndpointDescriptor\"},{\"old\":\"tools::execute_logged_action\",\"new\":\"tools::execute_action_logged\"}],\"question\":\"Are these renames correct and non-breaking?\",\"rationale\":\"Batch related renames to minimize rebuild cycles.\",\"predicted_next_actions\":[{\"action\":\"cargo_test\",\"intent\":\"Run tests after applying the batch rename.\"}]}\nNotes:\n- Symbol paths are module-relative (e.g. `tools::my_fn`). Crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted and stripped.\n- Uses `state/rustc/<crate>/graph.json` spans; if the graph is stale, the rename is rejected — rebuild then retry.\n- Safe by default: cargo check runs automatically after every rename. On failure the touched files are rolled back via git and compiler errors are written to `state/rename_errors.txt`. No manual cargo check step needed.",
            ),
        ),
        (
            "issue",
            "record/update discovered issues in ISSUES.json for later attention",
            Some(
                "Examples:\n  {\"action\":\"issue\",\"op\":\"read\",\"rationale\":\"Check open issues before starting work.\"}\n  {\"action\":\"issue\",\"op\":\"create\",\"evidence_receipts\":[\"rcpt-123-planner-1-read_file\"],\"issue\":{\"id\":\"ISS-001\",\"title\":\"Retry loop does not fire for submit-only turns\",\"status\":\"open\",\"priority\":\"high\",\"kind\":\"bug\",\"description\":\"...\",\"location\":\"src/ws_server.rs:554\",\"evidence\":[\"frames/inbound.jsonl fc=91 only presence frames after fc=76 heartbeat\"],\"discovered_by\":\"planner\"},\"rationale\":\"Record the stall bug for later fix using the current-cycle read receipt.\"}\n  {\"action\":\"issue\",\"op\":\"upsert\",\"evidence_receipts\":[\"rcpt-124-planner-2-python\"],\"issue\":{\"id\":\"ISS-001\",\"title\":\"Retry loop does not fire for submit-only turns\",\"status\":\"in_progress\",\"priority\":\"high\",\"kind\":\"bug\",\"description\":\"Updated issue body\"},\"rationale\":\"Legacy alias: create-or-replace the full issue payload by id.\"}\n  {\"action\":\"issue\",\"op\":\"set_status\",\"issue_id\":\"ISS-001\",\"status\":\"resolved\",\"evidence_receipts\":[\"rcpt-125-planner-3-python\"],\"rationale\":\"Issue was fixed by removing the pending check.\"}\n  {\"action\":\"issue\",\"op\":\"resolve\",\"issue_id\":\"ISS-001\",\"evidence_receipts\":[\"rcpt-126-planner-4-read_file\"],\"rationale\":\"Legacy alias: mark the issue resolved.\"}\n  {\"action\":\"issue\",\"op\":\"update\",\"issue_id\":\"ISS-001\",\"evidence_receipts\":[\"rcpt-127-planner-5-read_file\"],\"updates\":{\"priority\":\"medium\",\"description\":\"Updated description\"},\"rationale\":\"Revise issue details.\"}\nAllowed status: open | in_progress | resolved | wontfix\nAllowed priority: high | medium | low\nAllowed kind: bug | logic | invariant_violation | performance | stale_state\n⚠ Mutating issue ops (`create`, `update`, `set_status`, `upsert`, `resolve`) require non-empty `evidence_receipts` copied from a successful current-cycle `read_file`, `python`, or `run_command` result.",
            ),
        ),
        (
            "objectives",
            "read/update objectives in agent_state/OBJECTIVES.json",
            Some(
                "Examples:\n  {\"action\":\"objectives\",\"op\":\"read\",\"rationale\":\"Load only non-completed objectives for planning/verification.\"}\n  {\"action\":\"objectives\",\"op\":\"read\",\"include_done\":true,\"rationale\":\"Load all objectives, including completed.\"}\n  {\"action\":\"objectives\",\"op\":\"create_objective\",\"objective\":{\"id\":\"obj_new\",\"title\":\"New objective\",\"status\":\"active\",\"scope\":\"...\",\"authority_files\":[\"src/foo.rs\"],\"category\":\"quality\",\"level\":\"low\",\"description\":\"...\",\"requirement\":[],\"verification\":[],\"success_criteria\":[]},\"rationale\":\"Record a new objective.\"}\n  {\"action\":\"objectives\",\"op\":\"set_status\",\"objective_id\":\"obj_new\",\"status\":\"done\",\"rationale\":\"Mark objective complete.\"}\n  {\"action\":\"objectives\",\"op\":\"update_objective\",\"objective_id\":\"obj_new\",\"updates\":{\"scope\":\"updated scope\"},\"rationale\":\"Update objective fields.\"}\n  {\"action\":\"objectives\",\"op\":\"delete_objective\",\"objective_id\":\"obj_new\",\"rationale\":\"Remove obsolete objective.\"}\n  {\"action\":\"objectives\",\"op\":\"replace_objectives\",\"objectives\":[],\"rationale\":\"Replace objectives list.\"}\n  {\"action\":\"objectives\",\"op\":\"replace\",\"objectives\":[],\"rationale\":\"Legacy alias for replace_objectives; replace the objectives list.\"}\n  {\"action\":\"objectives\",\"op\":\"sorted_view\",\"rationale\":\"View objectives sorted by status.\"}",
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
                "Example:\n  {\"action\":\"python\",\"code\":\"from pathlib import Path\\nprint(len(list(Path('src').glob('**/*.rs'))))\",\"cwd\":\"/workspace/ai_sandbox/canon-mini-agent\",\"rationale\":\"Use Python for structured workspace analysis.\"}\n⚠ cwd may be relative to WORKSPACE or absolute under WORKSPACE.\n⚠ Successful `python` results now return an `Evidence receipt:` line; copy that `rcpt-*` id into later mutating `issue`/`violation` actions.",
            ),
        ),
        (
            "cargo_test",
            "run a targeted cargo test (harness-style)",
            Some(cargo_test_action_example()),
        ),
        (
            "cargo_fmt",
            "run cargo fmt (default: --check)",
            Some(
                "Examples:\n  {\"action\":\"cargo_fmt\",\"fix\":false,\"rationale\":\"Verify formatting without modifying files.\"}\n  {\"action\":\"cargo_fmt\",\"fix\":true,\"rationale\":\"Auto-format the workspace.\"}",
            ),
        ),
        (
            "cargo_clippy",
            "run cargo clippy -D warnings (optionally scoped to a crate)",
            Some(
                "Examples:\n  {\"action\":\"cargo_clippy\",\"rationale\":\"Lint the workspace with clippy.\"}\n  {\"action\":\"cargo_clippy\",\"crate\":\"canon-mini-agent\",\"rationale\":\"Lint only the target crate.\"}",
            ),
        ),
        (
            "plan",
            "create/update/delete tasks and DAG edges in PLAN.json",
            Some(plan_action_examples_block()),
        ),
        ("rustc_hir", "emit HIR for analysis", None),
        ("rustc_mir", "emit MIR for analysis", None),
        ("graph_call", "emit call graph CSVs", None),
        ("graph_cfg", "emit CFG CSVs", None),
        ("graph_dataflow", "emit dataflow reports", None),
        ("graph_reachability", "emit reachability reports", None),
        (
            "stage_graph",
            "emit a synthetic OODA-style stage graph (written to agent_state/orchestrator/stage_graph.json by default)",
            Some(
                "Example:\n  {\"action\":\"stage_graph\",\"rationale\":\"Generate the current stage graph for agent branching and introspection.\",\"predicted_next_actions\":[{\"action\":\"read_file\",\"intent\":\"Inspect the generated stage graph JSON.\"},{\"action\":\"semantic_map\",\"intent\":\"Jump from a stage anchor to code symbols.\"}]}\nNotes:\n- `out` defaults to `agent_state/orchestrator/stage_graph.json`.",
            ),
        ),
        (
            "semantic_map",
            "rustc-backed semantic graph triples: one `(from, relation, to)` line per edge",
            Some(
                "Examples:\n  {\"action\":\"semantic_map\",\"crate\":\"canon_mini_agent\",\"rationale\":\"Get compiler-backed semantic triples before exploring specific symbols.\"}\n  {\"action\":\"semantic_map\",\"crate\":\"canon_mini_agent\",\"filter\":\"tools\",\"rationale\":\"Restrict triples to edges touching the tools module.\"}\nNotes: returns one triple per line as `(from, relation, to)`. Symbol paths are module-relative (e.g. `tools::my_fn`). Crate-qualified prefixes like `canon_mini_agent::tools` or `crate::tools` are accepted and stripped. `filter` keeps triples whose source or target matches the prefix. `expand_bodies` is accepted for compatibility but ignored.",
            ),
        ),
        (
            "symbol_window",
            "extract the full definition body of a symbol (byte-precise, via def span)",
            Some(
                "Example:\n  {\"action\":\"symbol_window\",\"crate\":\"canon_mini_agent\",\"symbol\":\"tools::execute_logged_action\",\"rationale\":\"Read the exact body of a function before editing it.\"}\nNotes: `symbol` is module-relative (e.g. `tools::my_fn`). Crate-qualified prefixes like `canon_mini_agent::tools::my_fn` or `crate::tools::my_fn` are accepted and stripped. Accepts short unambiguous suffix if the full module path is unknown.",
            ),
        ),
        (
            "symbol_refs",
            "list all reference sites for a symbol; set expand_bodies:true to also show each enclosing function/struct/trait body (like symbol_window)",
            Some(
                "Example (sites only):\n  {\"action\":\"symbol_refs\",\"crate\":\"canon_mini_agent\",\"symbol\":\"tools::execute_logged_action\",\"rationale\":\"Find all call sites before changing a signature.\"}\nExample (with bodies):\n  {\"action\":\"symbol_refs\",\"crate\":\"canon_mini_agent\",\"symbol\":\"app::run_agent\",\"expand_bodies\":true,\"rationale\":\"Read every caller body to understand the call contract before refactoring.\"}\nNotes: `symbol` is module-relative; crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted and stripped. Covers every identifier span recorded by the HIR visitor during compilation. expand_bodies finds the tightest enclosing symbol in the graph and inlines its source.",
            ),
        ),
        (
            "symbol_path",
            "BFS shortest semantic-graph path between two symbols; set expand_bodies:true to inline the source body of each hop",
            Some(
                "Example:\n  {\"action\":\"symbol_path\",\"crate\":\"canon_mini_agent\",\"from\":\"app::run_agent\",\"to\":\"tools::handle_apply_patch_action\",\"rationale\":\"Trace the shortest semantic route between two symbols.\"}\nExample (with bodies):\n  {\"action\":\"symbol_path\",\"crate\":\"canon_mini_agent\",\"from\":\"app::run_agent\",\"to\":\"tools::handle_apply_patch_action\",\"expand_bodies\":true,\"rationale\":\"Read every symbol body along the semantic path before changing a handler signature.\"}\nNotes: `from`/`to` are module-relative; crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted and stripped. Traverses all semantic edges and labels each hop with its relation; returns the shortest path with file:line annotations.",
            ),
        ),
        (
            "execution_path",
            "BFS shortest unified path across semantic nodes, CFG blocks, and bridge edges",
            Some(
                "Example:\n  {\"action\":\"execution_path\",\"crate\":\"canon_mini_agent\",\"from\":\"app::run_agent\",\"to\":\"tools::handle_apply_patch_action\",\"rationale\":\"Trace the shortest execution-aware route between two symbols.\"}\nExample (to a raw CFG block):\n  {\"action\":\"execution_path\",\"crate\":\"canon_mini_agent\",\"from\":\"app::run_agent\",\"to\":\"cfg::app::run_agent::bb3\",\"rationale\":\"Reach a specific MIR basic block from the owning entry point.\"}\nNotes: `from`/`to` may be module-relative symbols or raw `cfg::...` node ids. Traverses semantic edges, CFG edges, and bridge edges (`Entry`, `BelongsTo`, `Call`). Returns the shortest path with relation labels.",
            ),
        ),
        (
            "symbol_neighborhood",
            "immediate callers and callees of a symbol; set expand_bodies:true to inline the source body of each caller and callee",
            Some(
                "Example:\n  {\"action\":\"symbol_neighborhood\",\"crate\":\"canon_mini_agent\",\"symbol\":\"tools::execute_logged_action\",\"rationale\":\"Understand the blast radius of a function before modifying it.\"}\nExample (with bodies):\n  {\"action\":\"symbol_neighborhood\",\"crate\":\"canon_mini_agent\",\"symbol\":\"tools::execute_logged_action\",\"expand_bodies\":true,\"rationale\":\"Read every caller and callee body before refactoring.\"}\nNotes: `symbol` is module-relative; crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted and stripped. Returns all direct callers and callees from the static call graph.",
            ),
        ),
        (
            "lessons",
            "review and promote detected action patterns into the lessons artifact injected into every planner prompt",
            Some(
                "Ops:\n  read_candidates — list pending patterns detected from the action log\n  promote — accept a candidate into lessons.json (entry status: pending)\n  reject  — discard a candidate permanently\n  encode  — mark a lessons.json entry as encoded into system source (removes it from prompt)\n  read    — view current lessons.json including entry statuses\n  write   — write a custom LessonsArtifact directly\n\nEntry status lifecycle:\n  pending → the lesson is injected into the planner prompt at runtime only\n  encoded → the lesson has been hardcoded into system source; excluded from prompt\n\nExamples:\n  {\"action\":\"lessons\",\"op\":\"read_candidates\",\"rationale\":\"See what patterns have been detected since the last synthesis run.\"}\n  {\"action\":\"lessons\",\"op\":\"promote\",\"candidate_id\":\"failure_abc123def\",\"rationale\":\"This failure pattern is real and recurring — promote to lessons.\"}\n  {\"action\":\"lessons\",\"op\":\"promote\",\"candidate_id\":\"all\",\"rationale\":\"All pending candidates are valid — bulk promote.\"}\n  {\"action\":\"lessons\",\"op\":\"reject\",\"candidate_id\":\"seq2_xyz\",\"rationale\":\"This sequence is coincidental, not a reliable workflow pattern.\"}\n  {\"action\":\"lessons\",\"op\":\"encode\",\"entry_text\":\"issue create: nest all fields under an `issue` key...\",\"rationale\":\"Added this check to schema_fix_hint() in src/lessons.rs — no longer needed in prompt.\"}\n  {\"action\":\"lessons\",\"op\":\"write\",\"lessons\":{\"summary\":\"...\",\"failures\":[{\"text\":\"...\",\"status\":\"pending\"}],\"fixes\":[],\"required_actions\":[]},\"rationale\":\"Write a hand-crafted lessons artifact from this cycle's findings.\"}"
            ),
        ),
        (
            "invariants",
            "review and enforce dynamically discovered system invariants; gates route/planner/executor dispatch on enforced invariants",
            Some(
                "Ops:\n  read    — view active invariants in enforced_invariants.json (discovered, promoted, enforced)\n  promote — upgrade a Discovered invariant to Promoted so it is checked by gates (id or \"all\")\n  enforce — upgrade a Promoted invariant to Enforced; the gate becomes hard-blocking\n  collapse — mark an invariant Collapsed when its root cause has been structurally eliminated\n\nInvariant status lifecycle:\n  discovered → synthesized from blockers.json/action log; support_count < threshold\n  promoted   → support_count >= threshold; gate checks it but does not block yet\n  enforced   → gate hard-blocks transitions that match the invariant's state_conditions\n  collapsed  → root cause structurally fixed; invariant retired (preserved for history)\n\nExamples:\n  {\"action\":\"invariants\",\"op\":\"read\",\"rationale\":\"Review which invariants are accumulating support or awaiting enforcement.\"}\n  {\"action\":\"invariants\",\"op\":\"promote\",\"id\":\"INV-a1b2c3d4\",\"rationale\":\"This pattern has strong support and the predicate is correct — promote it.\"}\n  {\"action\":\"invariants\",\"op\":\"enforce\",\"id\":\"INV-a1b2c3d4\",\"rationale\":\"Verified safe to hard-block: executor dispatched with no ready tasks is always wrong.\"}\n  {\"action\":\"invariants\",\"op\":\"collapse\",\"id\":\"INV-a1b2c3d4\",\"rationale\":\"Root cause eliminated in src/app.rs:1016 — invariant no longer needed.\"}"
            ),
        ),
        (
            "violation",
            "manage VIOLATIONS.json — add, update, resolve, or replace violation entries",
            Some(
                "Ops:\n  read       — read the current VIOLATIONS.json report\n  upsert     — add or replace a violation by id\n  resolve    — remove a violation by id (mark it fixed)\n  set_status — update report-level status and optional summary\n  replace    — replace the entire ViolationsReport\n\nViolation fields: id (string), title (string), severity (critical|high|medium|low), evidence (string[]), issue (string), impact (string), required_fix (string[]), files (string[]).\n\nExamples:\n  {\"action\":\"violation\",\"op\":\"read\",\"rationale\":\"Check current violations before deciding on next steps.\"}\n  {\"action\":\"violation\",\"op\":\"upsert\",\"evidence_receipts\":[\"rcpt-123-planner-1-read_file\"],\"violation\":{\"id\":\"PROMPT-OVERFLOW-PLANNER\",\"title\":\"Planner prompt exceeds token limit\",\"severity\":\"high\",\"evidence\":[\"prompt_bytes=23447\"],\"issue\":\"Planner prompt too large\",\"impact\":\"Model truncates context\",\"required_fix\":[\"Trim injected sections\"],\"files\":[]},\"rationale\":\"Add violation with current evidence.\"}\n  {\"action\":\"violation\",\"op\":\"resolve\",\"violation_id\":\"PROMPT-OVERFLOW-PLANNER\",\"evidence_receipts\":[\"rcpt-124-planner-2-python\"],\"rationale\":\"Prompt size reduced below threshold after trimming.\"}\n  {\"action\":\"violation\",\"op\":\"set_status\",\"status\":\"ok\",\"summary\":\"All prior violations resolved.\",\"evidence_receipts\":[\"rcpt-125-planner-3-run_command\"],\"rationale\":\"No active violations remain.\"}\n⚠ Mutating violation ops (`upsert`, `resolve`, `set_status`, `replace`) require non-empty `evidence_receipts` copied from a successful current-cycle `read_file`, `python`, or `run_command` result."
            ),
        ),
        (
            "batch",
            "execute up to 8 non-mutating actions in one turn; results returned as labeled sections",
            Some(
                "Example (read multiple files before patching):\n  {\"action\":\"batch\",\"rationale\":\"Gather all context needed before forming a patch.\",\"predicted_next_actions\":[{\"action\":\"apply_patch\",\"intent\":\"Apply the fix after reading all relevant code.\"},{\"action\":\"cargo_test\",\"intent\":\"Confirm fix compiles and tests pass.\"}],\"actions\":[{\"action\":\"read_file\",\"path\":\"src/app.rs\",\"line\":1800},{\"action\":\"symbol_window\",\"crate\":\"canon_mini_agent\",\"symbol\":\"app::apply_wake_flags\"},{\"action\":\"symbol_neighborhood\",\"crate\":\"canon_mini_agent\",\"symbol\":\"app::apply_wake_flags\"}]}\nExample (survey multiple modules):\n  {\"action\":\"batch\",\"rationale\":\"Map the relevant modules before a cross-cutting change.\",\"predicted_next_actions\":[{\"action\":\"semantic_map\",\"intent\":\"Drill into a specific module after surveying.\"}],\"actions\":[{\"action\":\"semantic_map\",\"crate\":\"canon_mini_agent\",\"filter\":\"tools\"},{\"action\":\"semantic_map\",\"crate\":\"canon_mini_agent\",\"filter\":\"app\"},{\"action\":\"list_dir\",\"path\":\"state\"}]}\nRules:\n- Max 8 items per batch.\n- Mutating actions (apply_patch, rename_symbol, message, run_command, python, cargo_test) are rejected.\n- For plan: only op=sorted_view is accepted.\n- For objectives: only op=read or op=sorted_view.\n- For issue: only op=read.\n- Items must omit rationale, predicted_next_actions, and observation.\n- On per-item error the item is labeled [batch N/M: ERROR] and execution continues.",
            ),
        ),
    ]
}

pub fn selected_tool_protocol_schema_text(actions: &[&str]) -> String {
    let schema = schema_for!(ToolAction);
    let value = serde_json::to_value(&schema).unwrap_or_else(|_| Value::Object(Default::default()));
    let mut out = String::new();
    out.push_str(&format!(
        "Only the schemas below are in scope for this turn. {} The `action` field must match one of these entries.\n",
        crate::prompt_contract::ACTION_EMIT_LINE
    ));
    out.push_str(
        "Common fields appear in every action: `rationale` (non-empty), `predicted_next_actions` (2-3 items), optional `observation`, and optional provenance fields `task_id`, `objective_id`, `intent`.\n\n",
    );

    let actions_meta = build_tool_actions_list();
    let mut rendered_any = false;
    let mut seen = std::collections::BTreeSet::new();
    for action in actions {
        if !seen.insert(*action) {
            continue;
        }
        let Some((_, desc, _notes)) = actions_meta.iter().find(|(name, _, _)| name == action)
        else {
            continue;
        };
        let schema = find_action_schema(&value, action)
            .and_then(|v| serde_json::to_string(v).ok())
            .unwrap_or_else(|| "{}".to_string());
        out.push_str(&format!(
            "Action: `{action}` — {desc}\n```json\n{schema}\n```\n\n"
        ));
        let mut derived_notes = Vec::new();
        match *action {
            "plan" => {
                let ops = schema_enum_values::<PlanOp>();
                if !ops.is_empty() {
                    derived_notes.push(format!(
                        "Schema-derived `plan.op` values: {}.",
                        ops.join(", ")
                    ));
                }
                derived_notes.push(
                    "Schema-derived reminder: use `add_edge` / `remove_edge` for DAG edges; do not emit `create_edge`.".to_string(),
                );
            }
            "issue" => {
                let ops = schema_enum_values::<IssueOp>();
                if !ops.is_empty() {
                    derived_notes.push(format!(
                        "Schema-derived `issue.op` values: {}.",
                        ops.join(", ")
                    ));
                }
            }
            "objectives" => {
                let ops = schema_enum_values::<ObjectivesOp>();
                if !ops.is_empty() {
                    derived_notes.push(format!(
                        "Schema-derived `objectives.op` values: {}.",
                        ops.join(", ")
                    ));
                }
            }
            _ => {}
        }
        if !derived_notes.is_empty() {
            out.push_str(&format!("{}\n\n", derived_notes.join("\n")));
        }
        rendered_any = true;
    }

    if !rendered_any {
        return String::new();
    }
    out
}

/// Write per-action syntax examples to `agent_state/tool_examples.md`.
/// Called once at agent-loop startup so the file is always fresh.
pub fn write_tool_examples(workspace: &std::path::Path) {
    if let Err(e) = write_tool_examples_inner(workspace) {
        eprintln!("[tool_examples] failed to write: {e}");
    }
}

fn write_tool_examples_inner(workspace: &std::path::Path) -> anyhow::Result<()> {
    let schema = schema_for!(ToolAction);
    let value = serde_json::to_value(&schema).unwrap_or_else(|_| Value::Object(Default::default()));
    let actions = build_tool_actions_list();
    let dir = workspace.join("agent_state");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("tool_examples.md");
    let mut out = String::from("# Tool action syntax examples\n\n");
    for (action, desc, notes) in &actions {
        out.push_str(&format!("## `{action}` — {desc}\n\n"));
        if let Some(notes) = notes {
            out.push_str(notes);
            out.push_str("\n\n");
        } else {
            let schema = find_action_schema(&value, action)
                .and_then(|v| serde_json::to_string(v).ok())
                .unwrap_or_else(|| "{}".to_string());
            out.push_str(&format!("```json\n{schema}\n```\n\n"));
        }
    }
    std::fs::write(&path, out)?;
    Ok(())
}

pub(crate) fn validate_tool_action(action: &Value) -> Result<()> {
    static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
    let compiled = SCHEMA.get_or_init(|| {
        let schema = schema_for!(ToolAction);
        let value = serde_json::to_value(&schema).expect("tool schema to value");
        JSONSchema::compile(&value).expect("compile tool schema")
    });
    let normalized_action = normalize_action_aliases_for_validation(action);
    validate_manual_length_guards(&normalized_action)?;
    if let Err(errors) = compiled.validate(&normalized_action) {
        let mut details = Vec::new();
        for err in errors.take(5) {
            details.push(map_schema_error_kind(
                &path_from_error(&err),
                &err.kind,
                &err.instance,
                &normalized_action,
            ));
        }
        let suffix = if details.is_empty() { "" } else { ": " };
        return Err(anyhow!(
            "action schema invalid{suffix}{}",
            details.join("; ")
        ));
    }
    // Manual guards not expressible in schemars 0.8
    if let Some(rationale) = normalized_action.get("rationale").and_then(|v| v.as_str()) {
        if rationale.trim().is_empty() {
            return Err(anyhow!("action missing non-empty 'rationale'"));
        }
        if rationale.chars().count() > ACTION_RATIONALE_MAX_LEN {
            return Err(anyhow!(
                "rationale exceeds max length ({ACTION_RATIONALE_MAX_LEN} chars)"
            ));
        }
    } else {
        return Err(anyhow!("action missing non-empty 'rationale'"));
    }
    if let Some(observation) = normalized_action.get("observation").and_then(|v| v.as_str()) {
        if observation.chars().count() > ACTION_OBSERVATION_MAX_LEN {
            return Err(anyhow!(
                "observation exceeds max length ({ACTION_OBSERVATION_MAX_LEN} chars)"
            ));
        }
    }
    if action_requires_question(&normalized_action) {
        let question = normalized_action
            .get("question")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if question.is_empty() {
            return Err(anyhow!(
                "mutating actions must include non-empty 'question'"
            ));
        }
    }
    let predicted = normalized_action
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
            return Err(anyhow!(
                "predicted_next_actions[{idx}] missing non-empty 'intent'"
            ));
        }
        if intent.chars().count() > PREDICTED_INTENT_MAX_LEN {
            return Err(anyhow!(
                "predicted_next_actions[{idx}].intent exceeds max length ({PREDICTED_INTENT_MAX_LEN} chars)"
            ));
        }
    }
    Ok(())
}

fn validate_manual_length_guards(action: &Value) -> Result<()> {
    if let Some(rationale) = action.get("rationale").and_then(|v| v.as_str()) {
        if rationale.trim().is_empty() {
            return Err(anyhow!("action missing non-empty 'rationale'"));
        }
        if rationale.chars().count() > ACTION_RATIONALE_MAX_LEN {
            return Err(anyhow!(
                "rationale exceeds max length ({ACTION_RATIONALE_MAX_LEN} chars)"
            ));
        }
    }
    if let Some(observation) = action.get("observation").and_then(|v| v.as_str()) {
        if observation.chars().count() > ACTION_OBSERVATION_MAX_LEN {
            return Err(anyhow!(
                "observation exceeds max length ({ACTION_OBSERVATION_MAX_LEN} chars)"
            ));
        }
    }
    if action_requires_question(action) {
        let question = action
            .get("question")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if question.is_empty() {
            return Err(anyhow!(
                "mutating actions must include non-empty 'question'"
            ));
        }
    }
    if let Some(predicted) = action.get("predicted_next_actions").and_then(|v| v.as_array()) {
        for (idx, item) in predicted.iter().enumerate() {
            let intent = item
                .get("intent")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or("");
            if intent.chars().count() > PREDICTED_INTENT_MAX_LEN {
                return Err(anyhow!(
                    "predicted_next_actions[{idx}].intent exceeds max length ({PREDICTED_INTENT_MAX_LEN} chars)"
                ));
            }
        }
    }
    Ok(())
}

fn action_requires_question(action: &Value) -> bool {
    let kind = action.get("action").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "apply_patch" | "rename_symbol" => true,
        "plan" => action.get("op").and_then(|v| v.as_str()) != Some("sorted_view"),
        "objectives" => !matches!(
            action.get("op").and_then(|v| v.as_str()),
            Some("read") | Some("sorted_view")
        ),
        "issue" => action.get("op").and_then(|v| v.as_str()) != Some("read"),
        _ => false,
    }
}

fn normalize_action_aliases_for_validation(action: &Value) -> Value {
    let mut normalized = action.clone();
    let Some(obj) = normalized.as_object_mut() else {
        return normalized;
    };
    if obj.get("action").and_then(|v| v.as_str()) != Some("plan") {
        return normalized;
    }
    let op = obj
        .get("op")
        .and_then(|v| v.as_str())
        .or_else(|| obj.get("operation").and_then(|v| v.as_str()));
    let Some(normalized_op) = op.and_then(normalize_plan_op_alias) else {
        return normalized;
    };
    if obj.get("op").is_some() {
        obj.insert("op".to_string(), Value::String(normalized_op.to_string()));
    }
    if obj.get("operation").is_some() {
        obj.insert(
            "operation".to_string(),
            Value::String(normalized_op.to_string()),
        );
    }
    normalized
}

fn normalize_plan_op_alias(op: &str) -> Option<&'static str> {
    match op {
        "create_edge" => Some("add_edge"),
        "delete_edge" => Some("remove_edge"),
        _ => None,
    }
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

pub(crate) fn action_schema_json(action: &str) -> Option<Value> {
    let schema = schema_for!(ToolAction);
    let value = serde_json::to_value(&schema).ok()?;
    let schema = find_action_schema(&value, action)?;
    Some(schema.clone())
}

fn map_schema_error(err: &jsonschema::error::ValidationError, action: &Value) -> String {
    let path = path_from_error(err);
    map_schema_error_kind(&path, &err.kind, &err.instance, action)
}

fn map_schema_error_kind(
    path: &str,
    kind: &ValidationErrorKind,
    instance: &Cow<'_, Value>,
    action: &Value,
) -> String {
    match kind {
        ValidationErrorKind::Required { property } => {
            missing_required_field_message(path, property.as_str().unwrap_or("unknown"))
        }
        ValidationErrorKind::Type { kind } => schema_type_mismatch_message(path, kind),
        ValidationErrorKind::MinLength { .. } => schema_missing_field_message(path),
        ValidationErrorKind::MaxLength { limit } => {
            let field = schema_error_field(path);
            format!("field too long: {field} (max {limit} chars)")
        }
        ValidationErrorKind::MinItems { .. } | ValidationErrorKind::MaxItems { .. } => {
            "predicted_next_actions must contain 2-3 entries".to_string()
        }
        ValidationErrorKind::Enum { .. } => enum_schema_error_message(path, instance, action),
        ValidationErrorKind::OneOfNotValid | ValidationErrorKind::AnyOf => {
            action_schema_mismatch_message(action)
        }
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            unexpected_fields_message(unexpected)
        }
        other => format!("schema violation: {other:?}"),
    }
}

fn schema_type_mismatch_message(path: &str, kind: &TypeKind) -> String {
    format!(
        "field type mismatch: {} (expected {})",
        schema_error_field(path),
        type_kind_label(kind)
    )
}

fn schema_missing_field_message(path: &str) -> String {
    format!("missing field: {}", schema_error_field(path))
}

fn unexpected_fields_message(unexpected: &[String]) -> String {
    format!("unexpected fields: {}", unexpected.join(", "))
}

fn schema_error_field(path: &str) -> String {
    if path.is_empty() {
        "action".to_string()
    } else {
        path.to_string()
    }
}

fn missing_required_field_message(path: &str, property: &str) -> String {
    if path.is_empty() {
        format!("missing field: {property}")
    } else {
        format!("{path}.{property} missing")
    }
}

fn enum_schema_error_message(path: &str, instance: &Cow<'_, Value>, _action: &Value) -> String {
    if path == "op" {
        format!("unknown plan op: {}", stringify_instance(instance))
    } else if path == "action" || path.ends_with(".action") {
        let action = stringify_instance(instance);
        if action == "symbol_search" {
            "unsupported action: symbol_search (use symbol_refs, symbol_window, symbol_path, symbol_neighborhood, or semantic_map)".to_string()
        } else {
            format!("unsupported action: {action}")
        }
    } else {
        format!("enum mismatch: {path}")
    }
}

fn action_schema_mismatch_message(action: &Value) -> String {
    let action_val = action.get("action").and_then(|v| v.as_str());
    if let Some(action_val) = action_val {
        if is_known_action(action_val) {
            if let Some(missing) = first_missing_field_for_action(action, action_val) {
                return missing;
            }
            return format!("action schema mismatch: {action_val}");
        }
        if action_val == "symbol_search" {
            return "unsupported action: symbol_search (use symbol_refs, symbol_window, symbol_path, symbol_neighborhood, or semantic_map)".to_string();
        }
        return format!("unsupported action: {action_val}");
    }
    "unsupported action: missing or unknown action".to_string()
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

    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        if matches_action(value, action) {
            return Some(value);
        }

        match value {
            Value::Array(arr) => {
                for item in arr.iter().rev() {
                    stack.push(item);
                }
            }
            Value::Object(map) => {
                let values: Vec<_> = map.values().collect();
                for item in values.into_iter().rev() {
                    stack.push(item);
                }
            }
            _ => {}
        }

        if let Some(arr) = value.get("anyOf").and_then(|v| v.as_array()) {
            for item in arr.iter().rev() {
                stack.push(item);
            }
        }
        if let Some(arr) = value.get("oneOf").and_then(|v| v.as_array()) {
            for item in arr.iter().rev() {
                stack.push(item);
            }
        }
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
            | "symbols_index"
            | "symbols_rename_candidates"
            | "symbols_prepare_rename"
            | "rename_symbol"
            | "objectives"
            | "issue"
            | "apply_patch"
            | "run_command"
            | "python"
            | "cargo_test"
            | "cargo_fmt"
            | "cargo_clippy"
            | "plan"
            | "semantic_map"
            | "symbol_window"
            | "symbol_refs"
            | "symbol_path"
            | "execution_path"
            | "symbol_neighborhood"
            | "rustc_hir"
            | "rustc_mir"
            | "graph_call"
            | "graph_cfg"
            | "graph_dataflow"
            | "graph_reachability"
            | "lessons"
            | "invariants"
            | "violation"
    )
}

fn first_missing_field_for_action(action: &Value, action_name: &str) -> Option<String> {
    let missing_field = |field: &str| missing_field_in_action(action, field);
    if action.get("predicted_next_actions").is_none() {
        return Some("missing field: predicted_next_actions".to_string());
    }
    fn require_fields<'a>(
        missing_field: &impl Fn(&str) -> Option<String>,
        fields: &[&'a str],
    ) -> Option<String> {
        for field in fields {
            if let Some(m) = missing_field(field) {
                return Some(m);
            }
        }
        None
    }

    fn require_one_of(
        action: &Value,
        missing_field: &impl Fn(&str) -> Option<String>,
    ) -> Option<String> {
        let has_bulk = action
            .get("renames")
            .and_then(|v| v.as_array())
            .is_some_and(|arr| !arr.is_empty());
        if has_bulk {
            None
        } else {
            missing_field("old_symbol").or_else(|| missing_field("new_symbol"))
        }
    }

    match action_name {
        "message" => require_fields(&missing_field, &["from", "to", "type", "status", "payload"]),
        "list_dir" | "read_file" => missing_field("path"),
        "symbols_index" | "symbols_rename_candidates" | "symbols_prepare_rename" => None,
        "rename_symbol" => require_one_of(action, &missing_field),
        "objectives" => missing_field_for_objectives_action(action),
        "issue" => missing_field_for_issue_action(action),
        "apply_patch" => missing_field("patch"),
        "run_command" => missing_field("cmd"),
        "python" => missing_field("code"),
        "cargo_test" => missing_field("crate"),
        "cargo_fmt" | "cargo_clippy" => None,
        "plan" => missing_field_for_plan_action(action),
        "invariants" => missing_field_for_invariants_action(action),
        "violation" => missing_field_for_violation_action(action),
        "rustc_hir" | "rustc_mir" | "graph_call" | "graph_cfg" | "graph_dataflow"
        | "graph_reachability" => missing_field("crate"),
        "semantic_map"
        | "symbol_window"
        | "symbol_refs"
        | "symbol_path"
        | "execution_path"
        | "symbol_neighborhood" => missing_field("crate"),
        _ => None,
    }
}

fn missing_field_in_action(action: &Value, field: &str) -> Option<String> {
    action
        .get(field)
        .is_none()
        .then(|| format!("missing field: {field}"))
}

fn missing_objective_id_field(action: &Value) -> Option<String> {
    if action.get("objective_id").is_none() && action.get("id").is_none() {
        Some("missing field: objective_id".to_string())
    } else {
        None
    }
}

fn missing_field_for_objectives_action(action: &Value) -> Option<String> {
    let op = action.get("op").and_then(|v| v.as_str()).unwrap_or("read");
    match op {
        "read" | "sorted_view" => None,
        "create_objective" => missing_field_in_action(action, "objective"),
        "update_objective" => missing_objective_id_field(action)
            .or_else(|| missing_field_in_action(action, "updates")),
        "delete_objective" => missing_objective_id_field(action),
        "set_status" => {
            missing_objective_id_field(action).or_else(|| missing_field_in_action(action, "status"))
        }
        "replace_objectives" | "replace" => missing_field_in_action(action, "objectives"),
        _ => None,
    }
}

fn missing_field_for_issue_action(action: &Value) -> Option<String> {
    let op = action.get("op").and_then(|v| v.as_str()).unwrap_or("read");
    match op {
        "read" => None,
        "create" | "upsert" => missing_field_in_action(action, "issue"),
        "update" => missing_field_in_action(action, "issue_id")
            .or_else(|| missing_field_in_action(action, "updates")),
        "delete" | "resolve" => missing_field_in_action(action, "issue_id"),
        "set_status" => missing_field_in_action(action, "issue_id")
            .or_else(|| missing_field_in_action(action, "status")),
        _ => None,
    }
}

fn missing_field_for_plan_action(action: &Value) -> Option<String> {
    let op = action
        .get("op")
        .and_then(|v| v.as_str())
        .or_else(|| action.get("operation").and_then(|v| v.as_str()))?;
    match op {
        "create_task" | "update_task" => missing_field_in_action(action, "task"),
        "delete_task" => missing_field_in_action(action, "task_id"),
        "add_edge" | "remove_edge" => missing_field_in_action(action, "from")
            .or_else(|| missing_field_in_action(action, "to")),
        "set_plan_status" => missing_field_in_action(action, "status"),
        "set_task_status" => missing_field_in_action(action, "task_id")
            .or_else(|| missing_field_in_action(action, "status")),
        "replace_plan" => missing_field_in_action(action, "plan"),
        "sorted_view" => None,
        _ => None,
    }
}

fn missing_field_for_violation_action(action: &Value) -> Option<String> {
    let op = action.get("op").and_then(|v| v.as_str()).unwrap_or("read");
    match op {
        "read" => None,
        "upsert" => missing_field_in_action(action, "violation"),
        "resolve" => missing_field_in_action(action, "violation_id"),
        "set_status" => missing_field_in_action(action, "status"),
        "replace" => missing_field_in_action(action, "report"),
        _ => None,
    }
}

fn missing_field_for_invariants_action(action: &Value) -> Option<String> {
    let op = action.get("op").and_then(|v| v.as_str()).unwrap_or("read");
    match op {
        "read" => None,
        "promote" | "enforce" | "collapse" => missing_field_in_action(action, "id"),
        _ => None,
    }
}
