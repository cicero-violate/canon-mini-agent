#![allow(dead_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Violation {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    pub evidence: Vec<String>,
    pub issue: String,
    pub impact: String,
    pub required_fix: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ViolationsReport {
    pub status: String,
    pub summary: String,
    pub violations: Vec<Violation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Impact {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsFinding {
    pub id: String,
    pub impact: Impact,
    pub signal: String,
    pub evidence: Vec<String>,
    #[serde(default)]
    pub root_cause: String,
    pub repair_targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsReport {
    pub status: String,
    pub inputs_scanned: Vec<String>,
    pub ranked_failures: Vec<DiagnosticsFinding>,
    pub planner_handoff: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvariantLevel {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvariantCategory {
    ControlLoop,
    RoutingAuthority,
    EventLogIntegrity,
    PolicyGating,
    Determinism,
    Planning,
    Safety,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invariant {
    pub id: String,
    pub title: String,
    pub category: InvariantCategory,
    pub level: InvariantLevel,
    pub description: String,
    pub clauses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantsReport {
    pub version: u32,
    pub invariants: Vec<Invariant>,
    pub principles: Vec<String>,
    pub math: Vec<String>,
    pub meta: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveLevel {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveCategory {
    EventBusIntegrity,
    HookSafety,
    ControlFlowGuarantee,
    DecisionDeterminism,
    AsyncPropagation,
    NoHiddenRouting,
    Instrumentation,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Objective {
    pub id: String,
    pub title: String,
    pub category: ObjectiveCategory,
    pub level: ObjectiveLevel,
    pub description: String,
    pub requirement: Vec<String>,
    pub verification: Vec<String>,
    pub success_criteria: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectivesReport {
    pub version: u32,
    pub objectives: Vec<Objective>,
    pub goal: Vec<String>,
    pub instrumentation: Vec<String>,
    pub definition_of_done: Vec<String>,
    pub non_goals: Vec<String>,
}
