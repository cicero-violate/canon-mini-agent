/// First-class blocker artifact.
///
/// Every bad outcome — whether an LLM-emitted `message{type=blocker}` or an
/// `ok=false` action result — is classified, recorded here, and becomes input
/// to the invariant synthesis pipeline.
///
/// ## Artifact
///
/// `agent_state/blockers.json` — append-only rolling log, capped at
/// `MAX_BLOCKER_RECORDS` entries.  Each record carries a structured
/// `error_class` field so invariant synthesis can group by class without
/// heuristic text matching.
///
/// ## Pipeline position
///
///   bad path → classify (error_class.rs) → append_blocker (here)
///     → invariant synthesis reads blockers.json
///     → groups by (actor_kind, error_class)
///     → promotes to invariant when support_count ≥ threshold
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::canonical_writer::CanonicalWriter;
use crate::error_class::ErrorClass;
use crate::events::{EffectEvent, Event};
use crate::logging::{record_effect_for_workspace, write_projection_with_artifact_effects};
use crate::tlog::Tlog;

// ── File path ─────────────────────────────────────────────────────────────────

const BLOCKERS_FILE: &str = "agent_state/blockers.json";

/// Keep at most this many records; oldest are dropped when the cap is reached.
const MAX_BLOCKER_RECORDS: usize = 500;

// ── Data structures ───────────────────────────────────────────────────────────

/// A single classified bad outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerRecord {
    /// Unique id derived from (actor_kind, error_class, ts_ms).
    pub id: String,
    /// Structured error class — the primary signal for invariant synthesis.
    pub error_class: ErrorClass,
    /// The role that emitted the bad outcome (executor, planner, verifier, …).
    pub actor: String,
    /// Task id in PLAN.json at the time, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Objective id at the time, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective_id: Option<String>,
    /// Free-text summary from the LLM or from the action result.
    pub summary: String,
    /// The action kind that produced this outcome (e.g. "read_file", "plan").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub action_kind: String,
    /// Source of the record: "blocker_message" | "action_result".
    pub source: String,
    /// Timestamp (ms) when this record was created.
    pub ts_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockersFile {
    pub version: u32,
    pub blockers: Vec<BlockerRecord>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Append a blocker record to `agent_state/blockers.json`.
///
/// Uses a file-level mutex to be safe under concurrent executor lanes.
/// Silently drops I/O errors to keep the hot path clean.
pub fn append_blocker(workspace: &Path, record: BlockerRecord) {
    if let Err(e) = try_append_blocker_with_writer(workspace, None, record) {
        eprintln!("[blockers] append error: {e:#}");
    }
}

/// Append a blocker record to `agent_state/blockers.json` using the canonical
/// writer when available.
pub fn append_blocker_with_writer(
    workspace: &Path,
    writer: Option<&mut CanonicalWriter>,
    record: BlockerRecord,
) -> Result<()> {
    try_append_blocker_with_writer(workspace, writer, record)
}

/// Convenience: build and append from a `message{type=blocker}` action.
pub fn record_blocker_message(
    workspace: &Path,
    role: &str,
    summary: &str,
    task_id: Option<&str>,
    objective_id: Option<&str>,
) {
    record_blocker_message_with_writer(workspace, None, role, summary, task_id, objective_id);
}

/// Convenience: build and append from a `message{type=blocker}` action.
pub fn record_blocker_message_with_writer(
    workspace: &Path,
    writer: Option<&mut CanonicalWriter>,
    role: &str,
    summary: &str,
    task_id: Option<&str>,
    objective_id: Option<&str>,
) {
    let error_class = crate::error_class::classify_blocker_summary(summary);
    if role == "diagnostics" && matches!(error_class, ErrorClass::BlockerEscalated) {
        return;
    }
    let ts = crate::logging::now_ms();
    let record = BlockerRecord {
        id: format!("blk-{role}-{}-{ts}", error_class.as_key()),
        error_class,
        actor: role.to_string(),
        task_id: task_id.map(str::to_string),
        objective_id: objective_id.map(str::to_string),
        summary: summary.to_string(),
        action_kind: "message".to_string(),
        source: "blocker_message".to_string(),
        ts_ms: ts,
    };
    if let Err(err) = append_blocker_with_writer(workspace, writer, record) {
        eprintln!("[blockers] append blocker message error: {err:#}");
    }
}

/// Convenience: build and append from an `ok=false` action result.
pub fn record_action_failure(
    workspace: &Path,
    role: &str,
    action_kind: &str,
    result_text: &str,
    task_id: Option<&str>,
) {
    record_action_failure_with_writer(workspace, None, role, action_kind, result_text, task_id);
}

/// Convenience: build and append from an `ok=false` action result.
pub fn record_action_failure_with_writer(
    workspace: &Path,
    writer: Option<&mut CanonicalWriter>,
    role: &str,
    action_kind: &str,
    result_text: &str,
    task_id: Option<&str>,
) {
    let error_class = crate::error_class::classify_result(action_kind, result_text, false);
    if error_class == ErrorClass::Unknown {
        return; // don't pollute blockers.json with unclassifiable noise
    }
    let ts = crate::logging::now_ms();
    let summary = result_text
        .lines()
        .next()
        .unwrap_or(result_text)
        .to_string();
    let record = BlockerRecord {
        id: format!("blk-{role}-{}-{ts}", error_class.as_key()),
        error_class,
        actor: role.to_string(),
        task_id: task_id.map(str::to_string),
        objective_id: None,
        summary,
        action_kind: action_kind.to_string(),
        source: "action_result".to_string(),
        ts_ms: ts,
    };
    if let Err(err) = append_blocker_with_writer(workspace, writer, record) {
        eprintln!("[blockers] append action failure error: {err:#}");
    }
}

/// Load the blockers file. Returns empty default if absent or unreadable.
pub fn load_blockers(workspace: &Path) -> BlockersFile {
    let path = blockers_path(workspace);
    if path.exists() {
        if let Some(file) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            return file;
        }
    }
    load_blockers_from_tlog(workspace).unwrap_or_default()
}

/// Count how many times a given (actor_kind, error_class) pair appears in the
/// blockers file.  Used by invariant synthesis instead of log text scanning.
pub fn count_class(file: &BlockersFile, actor_kind: &str, class: &ErrorClass) -> usize {
    file.blockers
        .iter()
        .filter(|b| b.actor.starts_with(actor_kind) && &b.error_class == class)
        .count()
}

/// Count how many times a given (actor_kind, error_class) pair appears within a
/// recent time window. Runtime gates can use this to react to active blocker
/// patterns without turning old blocker history into permanent poison state.
pub fn count_class_recent(
    file: &BlockersFile,
    actor_kind: &str,
    class: &ErrorClass,
    now_ms: u64,
    window_ms: u64,
) -> usize {
    file.blockers
        .iter()
        .filter(|b| {
            b.actor.starts_with(actor_kind)
                && &b.error_class == class
                && now_ms.saturating_sub(b.ts_ms) <= window_ms
        })
        .count()
}

// ── I/O internals ─────────────────────────────────────────────────────────────

fn blockers_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join(BLOCKERS_FILE)
}

fn load_blockers_from_tlog(workspace: &Path) -> Option<BlockersFile> {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let records = Tlog::read_records(&tlog_path).ok()?;
    let mut file = BlockersFile {
        version: 1,
        ..Default::default()
    };
    for record in records {
        let Event::Effect {
            event: EffectEvent::BlockerRecorded { record },
        } = record.event
        else {
            continue;
        };
        file.blockers.push(record);
    }
    if file.blockers.len() > MAX_BLOCKER_RECORDS {
        let drain_count = file.blockers.len() - MAX_BLOCKER_RECORDS;
        file.blockers.drain(0..drain_count);
    }
    Some(file)
}

fn try_append_blocker_with_writer(
    workspace: &Path,
    mut writer: Option<&mut CanonicalWriter>,
    record: BlockerRecord,
) -> Result<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let lock = LOCK.get_or_init(|| std::sync::Mutex::new(()));
    let _guard = lock
        .lock()
        .map_err(|_| anyhow::anyhow!("blockers mutex poisoned"))?;

    let path = blockers_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let effect = EffectEvent::BlockerRecorded {
        record: record.clone(),
    };
    if let Some(writer_ref) = writer.as_deref_mut() {
        writer_ref.try_record_effect(effect)?;
    } else {
        record_effect_for_workspace(workspace, effect)?;
    }
    let subject = record.id.clone();

    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut file: BlockersFile = if raw.trim().is_empty() {
        BlockersFile {
            version: 1,
            ..Default::default()
        }
    } else {
        serde_json::from_str(&raw).unwrap_or_else(|_| BlockersFile {
            version: 1,
            ..Default::default()
        })
    };

    file.blockers.push(record);

    // Cap to MAX_BLOCKER_RECORDS: keep the newest.
    if file.blockers.len() > MAX_BLOCKER_RECORDS {
        let drain_count = file.blockers.len() - MAX_BLOCKER_RECORDS;
        file.blockers.drain(0..drain_count);
    }

    write_projection_with_artifact_effects(
        workspace,
        &path,
        BLOCKERS_FILE,
        "append",
        &subject,
        &serde_json::to_string_pretty(&file)?,
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ws() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blockers_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn record_blocker_message_writes_file() {
        let ws = temp_ws();
        record_blocker_message(
            &ws,
            "executor_0",
            "cannot proceed: compile failed",
            None,
            None,
        );
        let file = load_blockers(&ws);
        assert_eq!(file.blockers.len(), 1);
        assert_eq!(file.blockers[0].error_class, ErrorClass::CompileError);
        assert_eq!(file.blockers[0].source, "blocker_message");
    }

    #[test]
    fn record_action_failure_skips_unknown() {
        let ws = temp_ws();
        record_action_failure(&ws, "executor_0", "read_file", "some generic output", None);
        // Generic output → Unknown class → should NOT be written.
        let file = load_blockers(&ws);
        assert_eq!(file.blockers.len(), 0);
    }

    #[test]
    fn record_action_failure_writes_classified() {
        let ws = temp_ws();
        record_action_failure(
            &ws,
            "executor_0",
            "apply_patch",
            "path is outside the permitted workspace",
            None,
        );
        let file = load_blockers(&ws);
        assert_eq!(file.blockers.len(), 1);
        assert_eq!(file.blockers[0].error_class, ErrorClass::PermissionDenied);
        assert_eq!(file.blockers[0].source, "action_result");
    }

    #[test]
    fn cap_enforced_on_overflow() {
        let ws = temp_ws();
        // Write MAX + 10 records.
        for i in 0..MAX_BLOCKER_RECORDS + 10 {
            let record = BlockerRecord {
                id: format!("blk-{i}"),
                error_class: ErrorClass::Unknown,
                actor: "executor".to_string(),
                task_id: None,
                objective_id: None,
                summary: "test".to_string(),
                action_kind: "message".to_string(),
                source: "blocker_message".to_string(),
                ts_ms: i as u64,
            };
            // Use the internal fn directly to bypass the Unknown filter.
            append_blocker(&ws, record);
        }
        let file = load_blockers(&ws);
        assert_eq!(file.blockers.len(), MAX_BLOCKER_RECORDS);
        // The newest records should be kept.
        assert_eq!(
            file.blockers.last().unwrap().ts_ms as usize,
            MAX_BLOCKER_RECORDS + 9
        );
    }

    #[test]
    fn count_class_groups_by_actor_and_class() {
        let ws = temp_ws();
        record_blocker_message(&ws, "executor_0", "compile failed", None, None);
        record_blocker_message(&ws, "executor_1", "cargo build failed", None, None);
        record_blocker_message(&ws, "planner", "compile failed", None, None);
        let file = load_blockers(&ws);
        let exec_compile = count_class(&file, "executor", &ErrorClass::CompileError);
        let plan_compile = count_class(&file, "planner", &ErrorClass::CompileError);
        assert_eq!(exec_compile, 2);
        assert_eq!(plan_compile, 1);
    }

    #[test]
    fn record_action_failure_writes_runtime_control_bypass() {
        let ws = temp_ws();
        record_action_failure(
            &ws,
            "orchestrate",
            "runtime_control_bypass",
            "runtime-only control influence: active blocker file suppressed planner dispatch",
            None,
        );
        let file = load_blockers(&ws);
        assert_eq!(file.blockers.len(), 1);
        assert_eq!(
            file.blockers[0].error_class,
            ErrorClass::RuntimeControlBypass
        );
    }

    #[test]
    fn record_action_failure_writes_uncanonicalized_recovery_path() {
        let ws = temp_ws();
        record_action_failure(
            &ws,
            "executor",
            "uncanonicalized_recovery",
            "recovery path without canonical event: late submit_ack reconstructed turn",
            None,
        );
        let file = load_blockers(&ws);
        assert_eq!(file.blockers.len(), 1);
        assert_eq!(
            file.blockers[0].error_class,
            ErrorClass::UncanonicalizedRecoveryPath
        );
    }
}
