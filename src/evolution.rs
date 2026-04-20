use crate::canonical_writer::CanonicalWriter;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvolutionSnapshot {
    #[serde(default)]
    pub evolution: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_build_ts_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_build_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvolutionAdvance {
    pub evolution: u64,
    pub command: String,
    pub git_commit: Option<String>,
    pub git_commit_count: Option<u64>,
}

pub fn snapshot_path_for_state_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("evolution.json")
}

pub fn current_evolution_for_tlog(tlog_path: &Path) -> u64 {
    tlog_path
        .parent()
        .map(load_snapshot_for_state_dir)
        .unwrap_or_default()
        .evolution
}

pub fn load_snapshot_for_state_dir(state_dir: &Path) -> EvolutionSnapshot {
    let path = snapshot_path_for_state_dir(state_dir);
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_snapshot_for_state_dir(state_dir: &Path, snapshot: &EvolutionSnapshot) -> Result<()> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create evolution state dir: {}", state_dir.display()))?;
    let path = snapshot_path_for_state_dir(state_dir);
    let raw = serde_json::to_string_pretty(snapshot)?;
    fs::write(&path, raw).with_context(|| format!("write evolution snapshot: {}", path.display()))
}

fn read_git_output(workspace: &Path, args: &[&str]) -> Option<String> {
    if !workspace.join(".git").exists() {
        return None;
    }
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

pub fn advance_for_successful_build(
    workspace: &Path,
    state_dir: &Path,
    command: &str,
) -> Result<EvolutionAdvance> {
    let mut snapshot = load_snapshot_for_state_dir(state_dir);
    snapshot.evolution = snapshot.evolution.saturating_add(1);
    snapshot.last_build_ts_ms = Some(crate::logging::now_ms());
    snapshot.last_build_command = Some(command.to_string());
    snapshot.git_commit = read_git_output(workspace, &["rev-parse", "HEAD"]);
    snapshot.git_commit_count = read_git_output(workspace, &["rev-list", "--count", "HEAD"])
        .and_then(|value| value.parse::<u64>().ok());
    save_snapshot_for_state_dir(state_dir, &snapshot)?;
    Ok(EvolutionAdvance {
        evolution: snapshot.evolution,
        command: command.to_string(),
        git_commit: snapshot.git_commit,
        git_commit_count: snapshot.git_commit_count,
    })
}

pub fn append_build_event_to_tlog(
    writer: &mut CanonicalWriter,
    advance: &EvolutionAdvance,
) -> Result<()> {
    writer.try_record_evolution_advance(advance)
}

pub fn record_successful_build(
    workspace: &Path,
    state_dir: &Path,
    command: &str,
    writer: &mut CanonicalWriter,
) -> Result<EvolutionAdvance> {
    let advance = advance_for_successful_build(workspace, state_dir, command)?;
    append_build_event_to_tlog(writer, &advance)?;
    Ok(advance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical_writer::CanonicalWriter;
    use crate::system_state::SystemState;
    use crate::tlog::Tlog;
    use serde_json::Value;

    fn tempdir(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "canon-evolution-test-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn record_successful_build_updates_snapshot_and_tlog() {
        let workspace = tempdir("workspace");
        let state_dir = workspace.join("agent_state");
        let tlog_path = state_dir.join("tlog.ndjson");
        let initial = SystemState::new(&[0], 1);
        let mut writer = CanonicalWriter::new(initial, Tlog::open(&tlog_path), workspace.clone());

        let advance = record_successful_build(
            &workspace,
            &state_dir,
            "cargo build --workspace",
            &mut writer,
        )
        .expect("record successful build");
        assert_eq!(advance.evolution, 1);
        assert!(advance.git_commit.is_none());

        let snapshot = load_snapshot_for_state_dir(&state_dir);
        assert_eq!(snapshot.evolution, 1);
        assert_eq!(
            snapshot.last_build_command.as_deref(),
            Some("cargo build --workspace")
        );

        let raw = fs::read_to_string(state_dir.join("tlog.ndjson")).expect("read tlog");
        let line = raw
            .lines()
            .find(|line| !line.trim().is_empty())
            .expect("tlog line");
        let value: Value = serde_json::from_str(line).expect("parse tlog line");
        assert_eq!(value.get("evolution").and_then(Value::as_u64), Some(1));
        assert_eq!(
            value.pointer("/event/class").and_then(Value::as_str),
            Some("effect")
        );
        assert_eq!(
            value.pointer("/event/event/kind").and_then(Value::as_str),
            Some("build_evolution_advanced")
        );
    }
}
