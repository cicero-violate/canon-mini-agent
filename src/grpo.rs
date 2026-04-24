use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingRow {
    pub group_key: String,
    pub command_id: String,
    pub state_seq: u64,
    pub prompt_hash: String,
    pub completion: String,
    pub result: String,
    pub action_kind: String,
    pub role: String,
    pub reward: f64,
    pub advantage: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupStats {
    pub mean: f64,
    pub std: f64,
    pub n: usize,
    pub outcome: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpoDataset {
    pub evolution: u64,
    pub generated_ms: u64,
    pub rows: Vec<TrainingRow>,
    pub group_stats: HashMap<String, GroupStats>,
}

#[derive(Debug, Default)]
struct PartialTurn {
    state_seq: Option<u64>,
    prompt_hash: Option<String>,
    role: Option<String>,
    completion: Option<String>,
    output_action_kind: Option<String>,
    result: Option<String>,
    result_action_kind: Option<String>,
    result_task_id: Option<String>,
    result_ok: Option<bool>,
}

#[derive(Debug, Clone)]
struct TaskOutcome {
    status: String,
    blocker_count: usize,
}

impl Default for TaskOutcome {
    fn default() -> Self {
        Self {
            status: "in_progress".to_string(),
            blocker_count: 0,
        }
    }
}

fn partial_turn_slot<'a>(
    by_command: &'a mut HashMap<String, PartialTurn>,
    command_id: String,
) -> &'a mut PartialTurn {
    by_command.entry(command_id).or_default()
}

fn assign_role_if_missing(slot: &mut PartialTurn, role: String) {
    if slot.role.is_none() {
        slot.role = Some(role);
    }
}

fn mark_task_blocked_unless_complete(outcomes: &mut HashMap<String, TaskOutcome>, task_id: String) {
    let entry = outcomes.entry(task_id).or_default();
    if entry.status != "complete" {
        entry.status = "blocked".to_string();
    }
}

fn record_failed_task_result(
    ok: bool,
    task_id: &Option<String>,
    outcomes: &mut HashMap<String, TaskOutcome>,
) {
    if ok {
        return;
    }
    let Some(task_id) = task_id else {
        return;
    };
    mark_task_blocked_unless_complete(outcomes, task_id.clone());
}

fn increment_task_blocker(
    blocker_count_by_task: &mut HashMap<String, usize>,
    task_id: Option<String>,
) {
    if let Some(task_id) = task_id {
        *blocker_count_by_task.entry(task_id).or_insert(0) += 1;
    }
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: u64, events::EffectEvent, &mut std::collections::HashMap<std::string::String, grpo::PartialTurn>, &mut std::collections::HashMap<std::string::String, grpo::TaskOutcome>, &mut std::collections::HashMap<std::string::String, usize>, &mut std::option::Option<drift_analysis::FingerprintDrift>
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn apply_effect_event(
    record_seq: u64,
    event: crate::events::EffectEvent,
    by_command: &mut HashMap<String, PartialTurn>,
    outcomes: &mut HashMap<String, TaskOutcome>,
    blocker_count_by_task: &mut HashMap<String, usize>,
    latest_drift: &mut Option<crate::drift_analysis::FingerprintDrift>,
) {
    match event {
        crate::events::EffectEvent::LlmTurnInput {
            role,
            command_id,
            prompt_hash,
            ..
        } => {
            let slot = partial_turn_slot(by_command, command_id);
            slot.state_seq = Some(record_seq);
            slot.prompt_hash = Some(prompt_hash);
            slot.role = Some(role);
        }
        crate::events::EffectEvent::LlmTurnOutput {
            role,
            command_id,
            raw,
            action_kind,
            ..
        } => {
            let slot = partial_turn_slot(by_command, command_id);
            slot.completion = Some(raw);
            slot.output_action_kind = action_kind;
            assign_role_if_missing(slot, role);
        }
        crate::events::EffectEvent::ActionResultRecorded {
            command_id,
            action_kind,
            task_id,
            ok,
            result,
            ..
        } => {
            let slot = partial_turn_slot(by_command, command_id);
            slot.result = Some(result);
            slot.result_action_kind = Some(action_kind);
            slot.result_task_id = task_id.clone();
            slot.result_ok = Some(ok);
            record_failed_task_result(ok, &task_id, outcomes);
        }
        crate::events::EffectEvent::InboundMessageRecorded { message, .. } => {
            update_outcome_from_message(&message, outcomes);
        }
        crate::events::EffectEvent::BlockerRecorded { record } => {
            increment_task_blocker(blocker_count_by_task, record.task_id);
        }
        crate::events::EffectEvent::FingerprintDriftRecorded { drift } => {
            *latest_drift = Some(drift);
        }
        _ => {}
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::path::Path, &std::path::Path
/// Outputs: std::result::Result<grpo::GrpoDataset, anyhow::Error>
/// Effects: fs_write, logging, state_write, transitions_state
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn extract_grpo_dataset(workspace: &Path, tlog_path: &Path) -> Result<GrpoDataset> {
    let records = crate::tlog::Tlog::read_records(tlog_path)
        .with_context(|| format!("read tlog records from {}", tlog_path.display()))?;

    let mut by_command: HashMap<String, PartialTurn> = HashMap::new();
    let mut outcomes: HashMap<String, TaskOutcome> = HashMap::new();
    let mut blocker_count_by_task: HashMap<String, usize> = HashMap::new();
    let mut latest_drift: Option<crate::drift_analysis::FingerprintDrift> = None;

    for record in records {
        let crate::events::Event::Effect { event } = record.event else {
            continue;
        };
        apply_effect_event(
            record.seq,
            event,
            &mut by_command,
            &mut outcomes,
            &mut blocker_count_by_task,
            &mut latest_drift,
        );
    }

    for (task_id, count) in blocker_count_by_task {
        outcomes.entry(task_id).or_default().blocker_count = count;
    }

    let drift_reward = latest_drift
        .as_ref()
        .map(normalized_drift_reward)
        .unwrap_or(0.0);

    let mut rows = Vec::new();
    for (command_id, turn) in by_command {
        let Some(state_seq) = turn.state_seq else {
            continue;
        };
        let Some(prompt_hash) = turn.prompt_hash else {
            continue;
        };
        let Some(completion) = turn.completion else {
            continue;
        };
        let Some(result) = turn.result else {
            continue;
        };

        let task_id = turn
            .result_task_id
            .or_else(|| parse_task_id_from_json(&completion))
            .unwrap_or_else(|| command_id.clone());
        let outcome = outcomes.get(&task_id).cloned().unwrap_or_default();
        let base_reward = match outcome.status.as_str() {
            "complete" => 1.0,
            "blocked" => -0.5 - (outcome.blocker_count as f64 * 0.05),
            _ => 0.0,
        };
        let reward = base_reward + (0.3 * drift_reward);

        rows.push(TrainingRow {
            group_key: task_id,
            command_id,
            state_seq,
            prompt_hash,
            completion,
            result,
            action_kind: turn
                .result_action_kind
                .or(turn.output_action_kind)
                .unwrap_or_else(|| "unknown".to_string()),
            role: turn.role.unwrap_or_else(|| "unknown".to_string()),
            reward,
            advantage: 0.0,
        });
    }

    rows.sort_by(|a, b| {
        a.group_key
            .cmp(&b.group_key)
            .then(a.state_seq.cmp(&b.state_seq))
    });

    let mut group_values: HashMap<String, Vec<f64>> = HashMap::new();
    for row in &rows {
        group_values
            .entry(row.group_key.clone())
            .or_default()
            .push(row.reward);
    }

    let mut group_stats: HashMap<String, GroupStats> = HashMap::new();
    for (group_key, values) in &group_values {
        if values.is_empty() {
            continue;
        }
        let n = values.len();
        let mean = values.iter().sum::<f64>() / n as f64;
        let var = values
            .iter()
            .map(|v| {
                let d = *v - mean;
                d * d
            })
            .sum::<f64>()
            / n.max(1) as f64;
        let std = var.sqrt();
        let outcome = outcomes
            .get(group_key)
            .map(|o| o.status.clone())
            .unwrap_or_else(|| "in_progress".to_string());
        group_stats.insert(
            group_key.clone(),
            GroupStats {
                mean,
                std,
                n,
                outcome,
            },
        );
    }

    for row in &mut rows {
        if let Some(stats) = group_stats.get(&row.group_key) {
            let denom = if stats.std < 0.01 { 1.0 } else { stats.std };
            row.advantage = (row.reward - stats.mean) / denom;
        }
    }

    let state_dir = workspace.join("agent_state");
    let evolution = crate::evolution::load_snapshot_for_state_dir(&state_dir).evolution;
    let dataset = GrpoDataset {
        evolution,
        generated_ms: crate::logging::now_ms(),
        rows,
        group_stats,
    };

    fs::create_dir_all(&state_dir).with_context(|| format!("create {}", state_dir.display()))?;
    let dataset_path = state_dir.join("grpo_dataset_latest.json");
    fs::write(&dataset_path, serde_json::to_string_pretty(&dataset)?)
        .with_context(|| format!("write {}", dataset_path.display()))?;

    let jsonl_path = state_dir.join("grpo_training_data.jsonl");
    let mut file =
        fs::File::create(&jsonl_path).with_context(|| format!("write {}", jsonl_path.display()))?;
    for row in &dataset.rows {
        let line = serde_json::to_string(row)?;
        writeln!(file, "{line}")?;
    }

    Ok(dataset)
}

pub fn record_grpo_dataset_effect(
    workspace: &Path,
    dataset: &GrpoDataset,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
) -> Result<()> {
    let mean_reward = if dataset.rows.is_empty() {
        0.0
    } else {
        dataset.rows.iter().map(|r| r.reward).sum::<f64>() / dataset.rows.len() as f64
    };
    let effect = crate::events::EffectEvent::GrpoDatasetRecorded {
        row_count: dataset.rows.len(),
        group_count: dataset.group_stats.len(),
        mean_reward,
    };

    if let Some(writer) = writer {
        writer.try_record_effect(effect)?;
    } else {
        crate::logging::record_effect_for_workspace(workspace, effect)?;
    }
    Ok(())
}

fn normalized_drift_reward(drift: &crate::drift_analysis::FingerprintDrift) -> f64 {
    let improved = drift.improved.len() as f64;
    let regressed = drift.regressed.len() as f64;
    let denom = (improved + regressed).max(1.0);
    (improved - 1.5 * regressed) / denom
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &str, &mut std::collections::HashMap<std::string::String, grpo::TaskOutcome>
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn update_outcome_from_message(message: &str, outcomes: &mut HashMap<String, TaskOutcome>) {
    let Some(json) = parse_json_payload(message) else {
        return;
    };
    let task_id = extract_task_id(&json);
    let status = extract_status(&json);
    let (Some(task_id), Some(status)) = (task_id, status) else {
        return;
    };
    let blocker_count = json
        .get("blocker_count")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;

    update_task_outcome(outcomes, task_id, status, blocker_count);
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &mut std::collections::HashMap<std::string::String, grpo::TaskOutcome>, std::string::String, std::string::String, usize
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn update_task_outcome(
    outcomes: &mut HashMap<String, TaskOutcome>,
    task_id: String,
    status: String,
    blocker_count: usize,
) {
    let entry = outcomes.entry(task_id).or_default();
    entry.status = status;
    if blocker_count > 0 {
        entry.blocker_count = blocker_count;
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_task_id_from_json(raw: &str) -> Option<String> {
    let value = parse_json_payload(raw)?;
    extract_task_id(&value)
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_task_id(value: &Value) -> Option<String> {
    value
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("payload")
                .and_then(|v| v.get("task_id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_status(value: &Value) -> Option<String> {
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| value.get("type").and_then(Value::as_str))?;
    let status = status.to_ascii_lowercase();
    match status.as_str() {
        "complete" | "blocked" | "in_progress" => Some(status),
        _ => None,
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::option::Option<serde_json::Value>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_json_payload(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }

    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.trim())
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    if let Ok(v) = serde_json::from_str::<Value>(unfenced) {
        return Some(v);
    }

    let start = unfenced.find('{')?;
    let end = unfenced.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&unfenced[start..=end]).ok()
}
