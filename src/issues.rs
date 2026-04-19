use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

use crate::constants::ISSUES_FILE;

pub fn persist_issues_projection_with_writer(
    workspace: &Path,
    file: &IssuesFile,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
    subject: &str,
) -> Result<()> {
    let effect = crate::events::EffectEvent::IssuesFileRecorded { file: file.clone() };
    if let Some(writer_ref) = writer {
        writer_ref.try_record_effect(effect)?;
    } else {
        crate::logging::record_effect_for_workspace(workspace, effect)?;
    }
    crate::logging::write_projection_with_artifact_effects(
        workspace,
        &workspace.join(ISSUES_FILE),
        ISSUES_FILE,
        "write",
        subject,
        &serde_json::to_string_pretty(file)?,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct IssuesFile {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub issues: Vec<Issue>,
}

/// A single issue recorded by any agent for later attention.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct Issue {
    /// Unique identifier, e.g. "ISS-001".
    pub id: String,
    /// Short human-readable title.
    pub title: String,
    /// open | in_progress | resolved | wontfix
    pub status: String,
    /// high | medium | low
    pub priority: String,
    /// Normalized priority score in [0.0, 1.0]. Auto-computed; do not set manually.
    /// Combines severity, recurrence, hot-path heuristic, and loop-velocity impact.
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub score: f32,
    /// bug | logic | invariant_violation | performance | stale_state
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// Full description of the issue and its impact.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// File path and/or component where the issue lives, e.g. "src/app.rs:420".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
    /// Structured metric payload for generator-emitted issues.
    #[serde(default, skip_serializing_if = "is_null_value")]
    pub metrics: Value,
    /// Scope of the issue, e.g. "crate:canon_mini_agent" or "state/rustc/.../graph.json".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
    /// Concrete acceptance criteria for closing the issue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_criteria: Vec<String>,
    /// Concrete evidence strings (log lines, test failures, frame data, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// Agent role that discovered this issue, e.g. "solo" or "diagnostics".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub discovered_by: String,
    /// fresh | stale | unknown
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

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_null_value(v: &Value) -> bool {
    v.is_null()
}

const ISSUE_FRESHNESS_TTL_MS: u64 = 15 * 60 * 1000;

#[derive(Debug, Clone, Default)]
pub struct IssueSweepSummary {
    pub marked_stale: usize,
    pub refreshed: usize,
    pub rewrote: bool,
}

pub fn load_issues_file(workspace: &Path) -> IssuesFile {
    let path = workspace.join(ISSUES_FILE);
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    if !raw.trim().is_empty() {
        if let Ok(file) = serde_json::from_str::<IssuesFile>(&raw) {
            return file;
        }
    }
    load_issues_from_tlog(workspace).unwrap_or_default()
}

fn load_issues_from_tlog(workspace: &Path) -> Option<IssuesFile> {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let records = crate::tlog::Tlog::read_records(&tlog_path).ok()?;
    let mut latest: Option<(u64, IssuesFile)> = None;
    for record in records {
        let crate::events::Event::Effect {
            event: crate::events::EffectEvent::IssuesFileRecorded { file },
        } = record.event
        else {
            continue;
        };
        let replace = match latest.as_ref() {
            None => true,
            Some((seq, _)) => record.seq >= *seq,
        };
        if replace {
            latest = Some((record.seq, file));
        }
    }
    latest.map(|(_, file)| file)
}

fn evidence_receipt_timestamps() -> HashMap<String, u64> {
    let path = Path::new(crate::constants::agent_state_dir()).join("evidence_receipts.jsonl");
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    raw.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| {
            Some((
                value.get("id")?.as_str()?.to_string(),
                value.get("ts_ms").and_then(|v| v.as_u64()).unwrap_or(0),
            ))
        })
        .collect()
}

fn normalize_issue_target_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_fragment = trimmed.split('#').next().unwrap_or(trimmed).trim();
    let without_note = without_fragment
        .split(" — ")
        .next()
        .unwrap_or(without_fragment)
        .trim();
    let candidate = without_note
        .split_whitespace()
        .next()
        .unwrap_or(without_note)
        .trim();
    if candidate.is_empty() {
        return None;
    }
    if let Some((head, tail)) = candidate.rsplit_once(':') {
        if !head.is_empty() && tail.chars().all(|ch| ch.is_ascii_digit() || ch == '-') {
            return Some(head.to_string());
        }
    }
    if candidate.starts_with('/')
        || candidate.contains('/')
        || candidate.ends_with(".json")
        || candidate.ends_with(".rs")
        || candidate.ends_with(".md")
    {
        return Some(candidate.to_string());
    }
    None
}

fn workspace_target_exists(workspace: &Path, raw: &str) -> Option<bool> {
    let normalized = normalize_issue_target_path(raw)?;
    let path = if normalized.starts_with('/') {
        std::path::PathBuf::from(normalized)
    } else {
        workspace.join(normalized)
    };
    Some(path.exists())
}

fn has_issue_freshness_metadata(issue: &Issue) -> bool {
    !issue.freshness_status.trim().is_empty()
        || issue.last_validated_ms > 0
        || !issue.stale_reason.trim().is_empty()
        || !issue.validated_from.is_empty()
        || !issue.evidence_receipts.is_empty()
        || !issue.evidence_hashes.is_empty()
}

fn collect_stale_reasons(
    issue: &Issue,
    workspace: &Path,
    receipt_ts: &HashMap<String, u64>,
    now_ms: u64,
) -> Vec<String> {
    let mut reasons = Vec::new();
    let has_freshness_metadata = has_issue_freshness_metadata(issue);

    if !issue.evidence_receipts.is_empty() {
        let mut missing = 0usize;
        let mut expired = 0usize;
        for receipt_id in &issue.evidence_receipts {
            match receipt_ts.get(receipt_id) {
                Some(ts_ms) if now_ms.saturating_sub(*ts_ms) <= ISSUE_FRESHNESS_TTL_MS => {}
                Some(_) => expired += 1,
                None => missing += 1,
            }
        }
        if missing == issue.evidence_receipts.len() {
            reasons.push("all evidence receipts missing".to_string());
        } else if missing > 0 {
            reasons.push(format!("{missing} evidence receipt(s) missing"));
        }
        if expired > 0 {
            reasons.push(format!("{expired} evidence receipt(s) expired"));
        }
    } else if issue.last_validated_ms > 0
        && now_ms.saturating_sub(issue.last_validated_ms) > ISSUE_FRESHNESS_TTL_MS
    {
        reasons.push("validation timestamp expired".to_string());
    }

    let mut validated_targets = 0usize;
    let mut missing_validated_targets = 0usize;
    for target in &issue.validated_from {
        if let Some(exists) = workspace_target_exists(workspace, target) {
            validated_targets += 1;
            if !exists {
                missing_validated_targets += 1;
            }
        }
    }
    if validated_targets > 0 && missing_validated_targets == validated_targets {
        reasons.push("validated_from targets missing".to_string());
    }

    if has_freshness_metadata {
        if let Some(false) = workspace_target_exists(workspace, &issue.location) {
            reasons.push("location target missing".to_string());
        }
    }

    reasons.sort();
    reasons.dedup();
    reasons
}

pub fn sweep_stale_issues(workspace: &Path) -> Result<IssueSweepSummary> {
    let mut file = load_issues_file(workspace);
    if file.issues.is_empty() {
        return Ok(IssueSweepSummary::default());
    }

    let receipt_ts = evidence_receipt_timestamps();
    let now_ms = crate::logging::now_ms();
    let mut summary = IssueSweepSummary::default();
    let mut mutated = false;

    for issue in &mut file.issues {
        if is_closed(issue) {
            continue;
        }
        let reasons = collect_stale_reasons(issue, workspace, &receipt_ts, now_ms);
        if !reasons.is_empty() {
            let joined = reasons.join("; ");
            if issue.freshness_status.trim().to_ascii_lowercase() != "stale"
                || issue.stale_reason != joined
            {
                issue.freshness_status = "stale".to_string();
                issue.stale_reason = joined;
                summary.marked_stale += 1;
                mutated = true;
            }
            continue;
        }

        let has_live_validation = !issue.evidence_receipts.is_empty()
            || issue.last_validated_ms > 0
            || !issue.validated_from.is_empty();
        if has_live_validation && issue.freshness_status.trim().to_ascii_lowercase() != "fresh" {
            issue.freshness_status = "fresh".to_string();
            issue.stale_reason.clear();
            summary.refreshed += 1;
            mutated = true;
        }
    }

    if mutated {
        rescore_all(&mut file);
        persist_issues_projection_with_writer(workspace, &file, None, "sweep_stale_issues")?;
        summary.rewrote = true;
    }
    Ok(summary)
}

pub fn issue_is_fresh(issue: &Issue) -> bool {
    if !has_issue_freshness_metadata(issue) {
        return true;
    }

    match issue.freshness_status.trim().to_ascii_lowercase().as_str() {
        "fresh" => return true,
        "stale" | "unknown" => return false,
        _ => {}
    }

    if issue.last_validated_ms > 0 {
        return true;
    }

    issue.evidence.iter().any(|entry| {
        let normalized = entry.to_ascii_lowercase();
        normalized.contains("validated against current source")
            || normalized.contains("current-cycle")
            || normalized.contains("read_file ")
            || normalized.contains("run_command ")
    })
}

/// Returns true for any status string that represents completion/closure.
/// Used both for issue filtering and plan task active-task-id tracking.
pub fn is_done_like_status(status: &str) -> bool {
    matches!(
        status.trim().to_lowercase().as_str(),
        "resolved" | "wontfix" | "done" | "complete" | "completed" | "verified" | "closed"
    )
}

pub fn is_closed(issue: &Issue) -> bool {
    is_done_like_status(&issue.status)
}

/// Compute a normalized [0.0, 1.0] priority score for an issue.
///
/// Weights:
///   severity      0.20 — priority string mapped to float
///   recurrence    0.15 — sibling issues with the same ID prefix (saturates at 3)
///   hot_path      0.25 — location/title mentions a per-turn code path
///   loop_velocity 0.30 — how much fixing this speeds up the agent's issue-close rate
///   scale         0.10 — candidate/instance count for auto-detected clusters (log2, saturates at ~128)
pub fn compute_issue_score(issue: &Issue, all_issues: &[Issue]) -> f32 {
    // Severity from priority string
    let severity: f32 = match issue.priority.trim().to_lowercase().as_str() {
        "critical" => 1.0,
        "high" => 0.75,
        "medium" => 0.5,
        "low" => 0.25,
        _ => 0.5,
    };

    // Recurrence: count other issues that share the same leading token in their ID.
    // IDs using hyphens as separators (e.g. "ISS-DUP-1" and "ISS-DUP-2") are siblings.
    // IDs using only underscores (e.g. auto_mir_dup_*) fall through with sibling_count=0,
    // which is correct — their cluster size is captured by the scale component instead.
    let base = issue.id.split('-').next().unwrap_or(&issue.id);
    let sibling_count = all_issues
        .iter()
        .filter(|i| i.id != issue.id && i.id.starts_with(base))
        .count();
    let recurrence = (sibling_count as f32 / 3.0).min(1.0);

    // Hot-path heuristic: is this in code that executes every agent turn?
    let combined = format!(
        "{} {} {}",
        issue.title.to_lowercase(),
        issue.description.to_lowercase(),
        issue.location.to_lowercase()
    );
    let hot_path_keywords = [
        "predicted_next_actions",
        "handle_batch",
        "canon-step",
        "canon_step",
        "every turn",
        "every cycle",
        "state_space",
        "dispatch",
    ];
    let hot_path: f32 = if hot_path_keywords.iter().any(|kw| combined.contains(kw)) {
        1.0
    } else {
        0.0
    };

    // Loop-velocity: how much does fixing this unblock the agent's self-improvement loop?
    let velocity: f32 = match issue.kind.trim().to_lowercase().as_str() {
        "bug" | "invariant_violation" => 1.0,
        "stale_state" | "logic" => 0.65,
        "performance" => 0.5,
        _ => {
            if issue.id.starts_with("auto_branch_reduce") || issue.id.starts_with("auto_refactor") {
                0.25
            } else {
                0.4
            }
        }
    };

    // Scale: candidate/instance count extracted from the issue title for auto-detected clusters
    // (e.g. "MIR-identical functions: 137 candidates for deduplication").
    // Uses log2 so the difference between 2 and 4 matters, but 64 vs 128 is marginal.
    // Saturates at 128 candidates (log2(128) = 7).
    let scale: f32 = {
        let n: u32 = issue
            .title
            .split_whitespace()
            .find_map(|w| w.parse().ok())
            .unwrap_or(1);
        if n > 1 {
            ((n as f32).log2() / 7.0).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };

    let score =
        0.20 * severity + 0.15 * recurrence + 0.25 * hot_path + 0.30 * velocity + 0.10 * scale;
    score.clamp(0.0, 1.0)
}

/// Recompute scores for every issue in the file.
/// Call this before writing so stored scores stay consistent.
pub fn rescore_all(file: &mut IssuesFile) {
    let snapshot = file.issues.clone();
    for issue in &mut file.issues {
        issue.score = compute_issue_score(issue, &snapshot);
    }
}

/// Read ISSUES.json and return the sorted, open/in-progress issues as structured data.
/// Returns an empty vector when the file is absent, invalid, or all issues are closed.
pub fn read_ranked_open_issues(workspace: &Path) -> Vec<Issue> {
    let _ = sweep_stale_issues(workspace);
    let mut file = load_issues_file(workspace);
    if file.issues.is_empty() {
        return Vec::new();
    }
    file.issues.retain(|i| !is_closed(i));
    file.issues.retain(issue_is_fresh);
    if file.issues.is_empty() {
        return Vec::new();
    }
    // Rescore on read so scores are always fresh even for old/unscored issues.
    rescore_all(&mut file);
    // Sort by score descending, then id for stability.
    file.issues.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    file.issues
}

#[cfg(test)]
mod tests {
    use super::{
        is_closed, load_issues_file, persist_issues_projection_with_writer,
        read_ranked_open_issues, sweep_stale_issues, Issue, IssuesFile,
    };
    use crate::{set_agent_state_dir, set_workspace};
    use std::path::Path;

    fn write_test_issue_file(path: &Path, raw: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create issues parent dir");
        }
        std::fs::write(path, raw).expect("write issues file");
    }

    fn render_open_issues(workspace: &Path) -> String {
        let issues = read_ranked_open_issues(workspace);
        if issues.is_empty() {
            return "(no open issues)".to_string();
        }
        let file = IssuesFile { version: 0, issues };
        serde_json::to_string_pretty(&file)
            .unwrap_or_else(|_| "(ISSUES.json is not valid JSON)".to_string())
    }

    fn render_top_open_issues(workspace: &Path, limit: usize) -> String {
        let issues = read_ranked_open_issues(workspace);
        if issues.is_empty() {
            return "(no open issues)".to_string();
        }
        let mut out = String::new();
        out.push_str("Top open issues:\n");
        let title_max_len = 120usize;
        let location_max_len = 80usize;
        let byte_budget = 4096usize;
        for issue in issues.into_iter().take(limit.max(1)) {
            let title = issue.title.trim();
            let truncated_title = if title.len() > title_max_len {
                format!("{}…", &title[..title_max_len])
            } else {
                title.to_string()
            };
            let location = issue.location.trim();
            let truncated_location = if location.is_empty() {
                String::new()
            } else if location.len() > location_max_len {
                format!("{}…", &location[..location_max_len])
            } else {
                location.to_string()
            };
            let loc = if truncated_location.is_empty() {
                String::new()
            } else {
                format!(" ({})", truncated_location)
            };
            let line = format!(
                "- [score:{:.2}] {}: {}{}\n",
                issue.score, issue.id, truncated_title, loc
            );
            if out.len() + line.len() > byte_budget {
                out.push_str("- … additional open issues omitted; use {\"action\":\"issue\",\"op\":\"read\"} for full detail\n");
                break;
            }
            out.push_str(&line);
        }
        out
    }

    #[test]
    fn is_closed_treats_done_like_statuses_as_closed() {
        for status in [
            "resolved",
            "wontfix",
            "done",
            "complete",
            "completed",
            "verified",
            "closed",
        ] {
            let issue = Issue {
                status: status.to_string(),
                ..Issue::default()
            };
            assert!(is_closed(&issue), "status should be closed: {status}");
        }
    }

    #[test]
    fn read_open_issues_filters_done_entries() {
        let root = std::env::temp_dir().join(format!(
            "canon-mini-agent-issues-test-{}",
            crate::logging::now_ms()
        ));
        std::fs::create_dir_all(&root).expect("create temp issues dir");
        let path = root.join(crate::constants::ISSUES_FILE);
        let raw = r#"{
  "version": 0,
  "issues": [
    { "id": "i_done", "title": "done issue", "status": "done", "priority": "high" },
    { "id": "i_open", "title": "open issue", "status": "open", "priority": "high" }
  ]
}"#;
        write_test_issue_file(&path, raw);
        let filtered = render_open_issues(&root);
        assert!(filtered.contains("\"id\": \"i_open\""));
        assert!(!filtered.contains("\"id\": \"i_done\""));
    }

    #[test]
    fn read_top_open_issues_returns_small_summary() {
        let root = std::env::temp_dir().join(format!(
            "canon-mini-agent-issues-top-test-{}",
            crate::logging::now_ms()
        ));
        std::fs::create_dir_all(&root).expect("create temp issues dir");
        let path = root.join(crate::constants::ISSUES_FILE);
        let raw = r#"{
  "version": 0,
  "issues": [
    { "id": "i_low", "title": "low issue", "status": "open", "priority": "low", "location": "a.rs:1" },
    { "id": "i_high", "title": "high issue", "status": "open", "priority": "high", "location": "b.rs:2" }
  ]
}"#;
        write_test_issue_file(&path, raw);
        let summary = render_top_open_issues(&root, 1);
        assert!(summary.contains("Top open issues"));
        assert!(summary.contains("i_high"));
        assert!(!summary.contains("i_low"));
    }

    #[test]
    fn sweep_stale_issues_marks_missing_receipt_issue_stale() {
        let root = std::env::temp_dir().join(format!(
            "canon-mini-agent-issues-sweep-test-{}",
            crate::logging::now_ms()
        ));
        std::fs::create_dir_all(&root).expect("create temp issues dir");
        let path = root.join(crate::constants::ISSUES_FILE);
        write_test_issue_file(
            &path,
            r#"{
  "version": 1,
  "issues": [
    {
      "id": "ISS-STALE",
      "title": "stale",
      "status": "open",
      "priority": "high",
      "freshness_status": "fresh",
      "evidence_receipts": ["rcpt-missing"],
      "last_validated_ms": 1
    }
  ]
}"#,
        );
        crate::constants::set_agent_state_dir(
            root.join("agent_state").to_string_lossy().into_owned(),
        );

        let summary = sweep_stale_issues(&root).expect("sweep should succeed");
        assert_eq!(summary.marked_stale, 1);
        let filtered = render_open_issues(&root);
        assert_eq!(filtered, "(no open issues)");
        let rewritten = std::fs::read_to_string(&path).expect("read rewritten file");
        assert!(rewritten.contains("\"freshness_status\": \"stale\""));
        assert!(rewritten.contains("all evidence receipts missing"));
    }

    #[test]
    fn sweep_stale_issues_keeps_plain_open_issue_without_validation_metadata() {
        let root = std::env::temp_dir().join(format!(
            "canon-mini-agent-issues-plain-open-test-{}",
            crate::logging::now_ms()
        ));
        std::fs::create_dir_all(&root).expect("create temp issues dir");
        let path = root.join(crate::constants::ISSUES_FILE);
        write_test_issue_file(
            &path,
            r#"{
  "version": 1,
  "issues": [
    {
      "id": "ISS-001",
      "title": "plain open issue",
      "status": "open",
      "priority": "high",
      "location": "missing.rs:10"
    }
  ]
}"#,
        );

        let summary = sweep_stale_issues(&root).expect("sweep should succeed");
        assert_eq!(summary.marked_stale, 0);
        assert!(!summary.rewrote);

        let rendered = render_open_issues(&root);
        assert!(rendered.contains("\"ISS-001\""));

        let top = render_top_open_issues(&root, 5);
        assert!(top.contains("Top open issues"));
        assert!(top.contains("ISS-001"));
    }

    #[test]
    fn load_issues_file_falls_back_to_latest_tlog_snapshot_when_projection_missing() {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _guard = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("lock");

        let root = std::env::temp_dir().join(format!(
            "canon-mini-agent-issues-tlog-{}",
            crate::logging::now_ms()
        ));
        let state_dir = root.join("agent_state");
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        set_workspace(root.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());

        let file = IssuesFile {
            version: 3,
            issues: vec![Issue {
                id: "ISS-TLOG-1".to_string(),
                title: "recover issues from tlog".to_string(),
                status: "open".to_string(),
                priority: "high".to_string(),
                description: "projection deleted; tlog snapshot should still drive reads"
                    .to_string(),
                ..Issue::default()
            }],
        };
        persist_issues_projection_with_writer(&root, &file, None, "issues_tlog_fallback_test")
            .expect("persist issues projection");

        let issues_path = root.join(crate::constants::ISSUES_FILE);
        std::fs::remove_file(&issues_path).expect("delete issues projection");

        let recovered = load_issues_file(&root);
        assert_eq!(recovered.version, 3);
        assert_eq!(recovered.issues.len(), 1);
        assert_eq!(recovered.issues[0].id, "ISS-TLOG-1");
        assert!(
            render_open_issues(&root).contains("ISS-TLOG-1"),
            "read surface should recover from the tlog snapshot"
        );
    }
}
