// Local workspace note: keep report types lightweight for graph/tlog correlation analysis.
use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::constants::diagnostics_file;

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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub freshness_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stale_reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validated_from: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_receipts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub last_validated_ms: u64,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

pub fn violation_is_fresh(v: &Violation) -> bool {
    let has_freshness_metadata = !v.freshness_status.trim().is_empty()
        || v.last_validated_ms > 0
        || !v.stale_reason.trim().is_empty()
        || !v.validated_from.is_empty()
        || !v.evidence_receipts.is_empty()
        || !v.evidence_hashes.is_empty();

    if !has_freshness_metadata {
        return true;
    }

    match v.freshness_status.trim().to_ascii_lowercase().as_str() {
        "fresh" => return true,
        "stale" | "unknown" => return false,
        _ => {}
    }

    if v.last_validated_ms > 0 {
        return true;
    }

    v.evidence.iter().any(|entry| {
        let normalized = entry.to_ascii_lowercase();
        normalized.contains("validated against current source")
            || normalized.contains("current-cycle")
            || normalized.contains("read_file ")
            || normalized.contains("run_command ")
    })
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

pub fn load_violations_report(workspace: &Path) -> ViolationsReport {
    let path = workspace.join(crate::constants::VIOLATIONS_FILE);
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    if !raw.trim().is_empty() {
        if let Ok(report) = serde_json::from_str::<ViolationsReport>(&raw) {
            return report;
        }
    }
    load_violations_from_tlog(workspace).unwrap_or(ViolationsReport {
        status: "ok".to_string(),
        summary: String::new(),
        violations: vec![],
    })
}

fn load_violations_from_tlog(workspace: &Path) -> Option<ViolationsReport> {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    crate::tlog::Tlog::latest_record_by_seq(&tlog_path, |event| match event {
        crate::events::Event::Effect {
            event: crate::events::EffectEvent::ViolationsReportRecorded { report },
        } => Some(report),
        _ => None,
    })
    .ok()?
}

pub fn persist_diagnostics_projection_with_writer_to_path(
    workspace: &Path,
    report: &DiagnosticsReport,
    target_path: &str,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
    subject: &str,
) -> Result<()> {
    crate::logging::record_json_projection_with_optional_writer(
        workspace,
        &workspace.join(target_path),
        target_path,
        "write",
        subject,
        report,
        writer,
        Some(crate::events::EffectEvent::DiagnosticsReportRecorded {
            report: report.clone(),
        }),
    )
}

pub fn load_diagnostics_report(workspace: &Path) -> Option<DiagnosticsReport> {
    let path = workspace.join(diagnostics_file());
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    if !raw.trim().is_empty() {
        if let Ok(report) = serde_json::from_str::<DiagnosticsReport>(&raw) {
            return Some(report);
        }
    }
    load_diagnostics_from_tlog(workspace)
}

fn load_diagnostics_from_tlog(workspace: &Path) -> Option<DiagnosticsReport> {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    crate::tlog::Tlog::latest_record_by_seq(&tlog_path, |event| match event {
        crate::events::Event::Effect {
            event: crate::events::EffectEvent::DiagnosticsReportRecorded { report },
        } => Some(report),
        _ => None,
    })
    .ok()?
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
