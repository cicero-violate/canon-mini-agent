use crate::canonical_writer::CanonicalWriter;
use crate::llm_runtime::config::LlmEndpoint;
use anyhow::{anyhow, bail, Context, Result};
use ra_ap_syntax::{AstNode, Edition, SourceFile, SyntaxKind, SyntaxToken};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{hash_map::DefaultHasher, BTreeSet};
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::canon_tools_patch::apply_patch;
use crate::constants::{
    diagnostics_file, is_self_modification_mode, ISSUES_FILE, MASTER_PLAN_FILE,
    MAX_FULL_READ_LINES, MAX_SNIPPET, OBJECTIVES_FILE, SPEC_FILE, VIOLATIONS_FILE,
};
use crate::events::ControlEvent;
use crate::issues::{is_closed, Issue, IssuesFile};
use crate::logging::{
    append_orchestration_trace, log_action_event, log_action_result, log_error_event, now_ms,
};
use crate::objectives::filter_incomplete_objectives_json;
use crate::prompts::truncate;
use crate::tool_schema::{
    plan_set_plan_status_action_example, plan_set_task_status_action_example,
};

// Split with `include!` to keep the module flat while reducing edit surface.
// Shards: foundation, objectives, issues, plan_view_io, symbols,
// patch_graph, plan_semantic, tail.
include!("tools_foundation.rs");
include!("tools_objectives.rs");
include!("tools_issues.rs");
include!("tools_plan_view_io.rs");
include!("tools_symbols.rs");
include!("tools_patch_graph.rs");
include!("tools_plan_semantic.rs");
include!("tools_tail.rs");
#[cfg(test)]
mod tests {
    use super::handle_apply_patch_action;
    use super::handle_execution_path_action;
    use super::handle_issue_action;
    use super::handle_objectives_action;
    use super::handle_plan_action;
    use super::handle_read_file_action;
    use super::handle_rename_symbol_action;
    use super::handle_stage_graph_action;
    use super::handle_symbols_index_action;
    use super::handle_symbols_prepare_rename_action;
    use super::handle_symbols_rename_candidates_action;
    use super::is_allowed_self_addressed_message;
    use super::stable_hash_hex;
    use super::EvidenceReceipt;
    use crate::constants::set_agent_state_dir;
    use crate::constants::set_workspace;
    use crate::constants::{ISSUES_FILE, MASTER_PLAN_FILE};
    use crate::issues::IssuesFile;
    use crate::logging::init_log_paths;
    use crate::logging::now_ms;
    use serde_json::json;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::OnceLock;

    fn test_state_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn write_minimal_graph_for_ident(
        workspace: &std::path::Path,
        crate_name: &str,
        symbol_key: &str,
        file: &std::path::Path,
        source: &str,
        ident: &str,
    ) {
        let mut refs = Vec::new();
        for (lo, _) in source.match_indices(ident) {
            let hi = lo + ident.len();
            let prefix = &source[..lo];
            let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
            let col = prefix.bytes().rev().take_while(|b| *b != b'\n').count();
            refs.push(serde_json::json!({
                "file": file.display().to_string(),
                "line": line as u32,
                "col": col as u32,
                "lo": lo as u32,
                "hi": hi as u32,
            }));
        }
        let graph = serde_json::json!({
            "nodes": {
                symbol_key: {
                    "kind": "fn",
                    "refs": refs,
                    "fields": [],
                }
            },
            "edges": []
        });
        let path = workspace
            .join("state/rustc")
            .join(crate_name)
            .join("graph.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&graph).unwrap()).unwrap();
    }

    fn fresh_test_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("canon-mini-agent-{name}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("agent_state")).unwrap();
        dir
    }

    fn write_test_evidence_receipt(
        workspace: &std::path::Path,
        state_dir: &std::path::Path,
        id: &str,
    ) {
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());
        std::fs::create_dir_all(state_dir).unwrap();
        let receipt = EvidenceReceipt {
            id: id.to_string(),
            ts_ms: now_ms(),
            actor: "planner".to_string(),
            step: 1,
            action: "python".to_string(),
            path: Some(ISSUES_FILE.to_string()),
            abs_path: Some(workspace.join(ISSUES_FILE).display().to_string()),
            meta: json!({"test": true}),
            output_hash: stable_hash_hex("test-output"),
        };
        std::fs::write(
            state_dir.join("evidence_receipts.jsonl"),
            format!("{}\n", serde_json::to_string(&receipt).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn issue_upsert_alias_creates_and_updates_issue() {
        let _guard = test_state_lock().lock().unwrap();
        let workspace = fresh_test_dir("issue-upsert");
        let state_dir = workspace.join("agent_state");
        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-upsert-1");

        let create = json!({
            "action": "issue",
            "op": "upsert",
            "evidence_receipts": ["rcpt-issue-upsert-1"],
            "issue": {
                "id": "ISS-transport",
                "title": "Transport failure",
                "status": "open",
                "priority": "high",
                "kind": "bug",
                "description": "first version",
                "evidence": ["tlog blocker"],
                "discovered_by": "planner"
            },
            "rationale": "record runtime blocker",
            "predicted_next_actions": []
        });
        let (_, create_out) = handle_issue_action(None, &workspace, &create).unwrap();
        assert!(create_out.contains("added `ISS-transport`"));

        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-upsert-2");
        let update = json!({
            "action": "issue",
            "op": "upsert",
            "issue_id": "ISS-transport",
            "evidence_receipts": ["rcpt-issue-upsert-2"],
            "issue": {
                "id": "ISS-transport",
                "title": "Transport failure",
                "status": "in_progress",
                "priority": "medium",
                "kind": "bug",
                "description": "updated version",
                "evidence": ["tlog blocker", "fresh receipt"],
                "discovered_by": "planner"
            },
            "rationale": "refresh runtime blocker",
            "predicted_next_actions": []
        });
        let (_, update_out) = handle_issue_action(None, &workspace, &update).unwrap();
        assert!(update_out.contains("updated `ISS-transport`"));

        let issues: IssuesFile =
            serde_json::from_str(&std::fs::read_to_string(workspace.join(ISSUES_FILE)).unwrap())
                .unwrap();
        let issue = issues
            .issues
            .iter()
            .find(|issue| issue.id == "ISS-transport")
            .unwrap();
        assert_eq!(issue.status, "in_progress");
        assert_eq!(issue.priority, "medium");
        assert_eq!(issue.description, "updated version");
    }

    #[test]
    fn issue_resolve_alias_marks_issue_resolved() {
        let _guard = test_state_lock().lock().unwrap();
        let workspace = fresh_test_dir("issue-resolve");
        let state_dir = workspace.join("agent_state");
        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-resolve-1");

        let create = json!({
            "action": "issue",
            "op": "create",
            "evidence_receipts": ["rcpt-issue-resolve-1"],
            "issue": {
                "id": "ISS-recover",
                "title": "Recovered completion",
                "status": "open",
                "priority": "high",
                "kind": "bug",
                "description": "needs closure",
                "evidence": ["tlog blocker"],
                "discovered_by": "planner"
            },
            "rationale": "seed issue",
            "predicted_next_actions": []
        });
        handle_issue_action(None, &workspace, &create).unwrap();

        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-resolve-2");
        let resolve = json!({
            "action": "issue",
            "op": "resolve",
            "issue_id": "ISS-recover",
            "evidence_receipts": ["rcpt-issue-resolve-2"],
            "rationale": "close resolved issue",
            "predicted_next_actions": []
        });
        let (_, out) = handle_issue_action(None, &workspace, &resolve).unwrap();
        assert!(out.contains("issue resolve ok"));

        let issues: IssuesFile =
            serde_json::from_str(&std::fs::read_to_string(workspace.join(ISSUES_FILE)).unwrap())
                .unwrap();
        let issue = issues
            .issues
            .iter()
            .find(|issue| issue.id == "ISS-recover")
            .unwrap();
        assert_eq!(issue.status, "resolved");
    }

    #[test]
    fn issue_update_auto_creates_missing_issue_stub() {
        let _guard = test_state_lock().lock().unwrap();
        let workspace = fresh_test_dir("issue-update-auto-create");
        let state_dir = workspace.join("agent_state");
        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-update-auto-create");

        let update = json!({
            "action": "issue",
            "op": "update",
            "issue_id": "auto_missing_issue_update",
            "updates": {
                "title": "Recovered missing issue",
                "status": "in_progress",
                "description": "auto-created during update"
            },
            "evidence_receipts": ["rcpt-issue-update-auto-create"],
            "rationale": "recover missing issue ids without hard failure",
            "predicted_next_actions": []
        });
        let (_, out) = handle_issue_action(None, &workspace, &update).unwrap();
        assert!(out.contains("issue update ok"));

        let issues: IssuesFile =
            serde_json::from_str(&std::fs::read_to_string(workspace.join(ISSUES_FILE)).unwrap())
                .unwrap();
        let issue = issues
            .issues
            .iter()
            .find(|issue| issue.id == "auto_missing_issue_update")
            .unwrap();
        assert_eq!(issue.status, "in_progress");
        assert_eq!(issue.title, "Recovered missing issue");
        assert_eq!(issue.description, "auto-created during update");
    }

    #[test]
    fn issue_set_status_auto_creates_missing_issue_stub() {
        let _guard = test_state_lock().lock().unwrap();
        let workspace = fresh_test_dir("issue-set-status-auto-create");
        let state_dir = workspace.join("agent_state");
        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-set-status-auto-create");

        let set_status = json!({
            "action": "issue",
            "op": "set_status",
            "issue_id": "auto_missing_issue_status",
            "status": "resolved",
            "evidence_receipts": ["rcpt-issue-set-status-auto-create"],
            "rationale": "recover missing issue ids without blocker loops",
            "predicted_next_actions": []
        });
        let (_, out) = handle_issue_action(None, &workspace, &set_status).unwrap();
        assert!(out.contains("issue set_status ok"));

        let issues: IssuesFile =
            serde_json::from_str(&std::fs::read_to_string(workspace.join(ISSUES_FILE)).unwrap())
                .unwrap();
        let issue = issues
            .issues
            .iter()
            .find(|issue| issue.id == "auto_missing_issue_status")
            .unwrap();
        assert_eq!(issue.status, "resolved");
        assert_eq!(issue.kind, "stale_state");
    }

    #[test]
    fn issue_resolve_missing_issue_is_noop() {
        let _guard = test_state_lock().lock().unwrap();
        let workspace = fresh_test_dir("issue-resolve-missing-noop");
        let state_dir = workspace.join("agent_state");
        write_test_evidence_receipt(&workspace, &state_dir, "rcpt-issue-resolve-missing-noop");

        let resolve = json!({
            "action": "issue",
            "op": "resolve",
            "issue_id": "auto_missing_issue_resolve",
            "evidence_receipts": ["rcpt-issue-resolve-missing-noop"],
            "rationale": "allow idempotent resolve against missing ids",
            "predicted_next_actions": []
        });
        let (_, out) = handle_issue_action(None, &workspace, &resolve).unwrap();
        assert!(out.contains("already absent"));

        let issues_path = workspace.join(ISSUES_FILE);
        let issues: IssuesFile = if issues_path.exists() {
            serde_json::from_str(&std::fs::read_to_string(&issues_path).unwrap()).unwrap()
        } else {
            IssuesFile::default()
        };
        assert!(issues
            .issues
            .iter()
            .all(|issue| issue.id != "auto_missing_issue_resolve"));
    }

    #[test]
    fn only_solo_result_complete_may_self_route() {
        let solo = json!({
            "action": "message",
            "from": "solo",
            "to": "solo",
            "type": "result",
            "status": "complete",
            "payload": {"summary": "done"}
        });
        assert!(is_allowed_self_addressed_message(&solo, "solo", "solo"));

        let planner = json!({
            "action": "message",
            "from": "planner",
            "to": "planner",
            "type": "blocker",
            "status": "blocked",
            "payload": {"summary": "blocked"}
        });
        assert!(!is_allowed_self_addressed_message(
            &planner, "planner", "planner"
        ));
    }

    fn write_minimal_graph_with_def_and_mir(
        workspace: &std::path::Path,
        crate_name: &str,
        symbol_key: &str,
        file: &std::path::Path,
        source: &str,
        ident: &str,
    ) {
        let lo = source.find(ident).expect("ident present");
        let hi = lo + ident.len();
        let prefix = &source[..lo];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
        let col = prefix.bytes().rev().take_while(|b| *b != b'\n').count();
        let def = serde_json::json!({
            "file": file.display().to_string(),
            "line": line as u32,
            "col": col as u32,
            "lo": lo as u32,
            "hi": hi as u32,
        });
        let graph = serde_json::json!({
            "nodes": {
                symbol_key: {
                    "kind": "fn",
                    "def": def,
                    "refs": [],
                    "signature": "fn test()",
                    "mir": { "fingerprint": "fp1", "blocks": 2, "stmts": 3 },
                    "fields": [],
                }
            },
            "edges": []
        });
        let path = workspace
            .join("state/rustc")
            .join(crate_name)
            .join("graph.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&graph).unwrap()).unwrap();
    }

    #[test]
    fn diagnostics_apply_patch_rejects_unvalidated_ranked_failures() {
        let tmp = fresh_test_dir("rejects-unvalidated-ranked-failures");
        std::fs::write(
            tmp.join("DIAGNOSTICS.json"),
            "{\"status\":\"healthy\",\"ranked_failures\":[]}",
        )
        .unwrap();
        let action = json!({
            "patch": "*** Begin Patch\n*** Delete File: DIAGNOSTICS.json\n*** Add File: DIAGNOSTICS.json\n+{\n+  \"status\": \"critical_failure\",\n+  \"summary\": \"stale issue\",\n+  \"ranked_failures\": [\n+    {\n+      \"id\": \"D1\",\n+      \"evidence\": [\"old report without source validation\"]\n+    }\n+  ]\n+}\n*** End Patch"
        });

        let (_done, out) =
            handle_apply_patch_action("diagnostics", 1, None, &tmp, &action).unwrap();

        assert!(out.contains("derived cache view"));
        let persisted: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.join("DIAGNOSTICS.json")).unwrap())
                .unwrap();
        assert_eq!(
            persisted.get("status").and_then(|v| v.as_str()),
            Some("healthy")
        );
        assert_eq!(
            persisted
                .get("ranked_failures")
                .and_then(|v| v.as_array())
                .map(|entries| entries.len()),
            Some(0)
        );
    }

    #[test]
    fn read_file_result_surfaces_evidence_receipt_id() {
        let tmp = fresh_test_dir("read-file-receipt");
        let target = tmp.join("sample.txt");
        std::fs::write(&target, "alpha\nbeta\n").unwrap();

        let action = json!({"path": "sample.txt"});
        let (_done, out) = handle_read_file_action("diagnostics", 1, &tmp, &action).unwrap();

        assert!(out.contains("Evidence receipt: rcpt-"), "unexpected: {out}");
        assert!(out.contains("alpha"), "unexpected: {out}");
    }

    #[test]
    fn stage_graph_writes_default_artifact() {
        let tmp = fresh_test_dir("stage-graph");
        init_log_paths("stage-graph-test");
        let action = json!({});
        let (_done, out) = handle_stage_graph_action(&tmp, &action).unwrap();
        assert!(out.contains("\"nodes\""));
        assert!(out.contains("observe.input"));
        let path = tmp.join("agent_state/orchestrator/stage_graph.json");
        assert!(path.exists(), "expected stage graph at {}", path.display());
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            parsed
                .get("nodes")
                .and_then(|v| v.as_array())
                .unwrap()
                .len()
                >= 6
        );
        assert!(
            parsed
                .get("edges")
                .and_then(|v| v.as_array())
                .unwrap()
                .len()
                >= 7
        );
    }

    #[test]
    fn diagnostics_apply_patch_allows_source_validated_ranked_failures() {
        let tmp = fresh_test_dir("allows-source-validated-ranked-failures");
        std::fs::write(
            tmp.join("DIAGNOSTICS.json"),
            r#"{"status":"healthy","inputs_scanned":[],"ranked_failures":[],"planner_handoff":[]}"#,
        )
        .unwrap();
        let action = json!({
            "patch": "*** Begin Patch\n*** Delete File: DIAGNOSTICS.json\n*** Add File: DIAGNOSTICS.json\n+{\n+  \"status\": \"critical_failure\",\n+  \"inputs_scanned\": [\"agent_state/default/log.jsonl\"],\n+  \"ranked_failures\": [\n+    {\n+      \"id\": \"D1\",\n+      \"impact\": \"high\",\n+      \"signal\": \"read_file src/app.rs verified against current source\",\n+      \"evidence\": [\"read_file src/app.rs:1-50 — confirmed missing check\"],\n+      \"root_cause\": \"missing validation\",\n+      \"repair_targets\": [\"src/app.rs\"]\n+    }\n+  ],\n+  \"planner_handoff\": [\"Fix missing validation in src/app.rs\"]\n+}\n*** End Patch"
        });

        let (_done, out) =
            handle_apply_patch_action("diagnostics", 1, None, &tmp, &action).unwrap();

        assert!(out.contains("derived cache view"), "unexpected: {out}");
        let persisted: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.join("DIAGNOSTICS.json")).unwrap())
                .unwrap();
        assert_eq!(
            persisted.get("status").and_then(|v| v.as_str()),
            Some("healthy")
        );
        assert_eq!(
            persisted
                .get("ranked_failures")
                .and_then(|v| v.as_array())
                .map(|entries| entries.len()),
            Some(0)
        );
    }

    #[test]
    fn execution_path_persists_latest_plan_artifact() {
        let tmp = fresh_test_dir("execution-path-artifact");
        let file = tmp.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let src = "fn validate() {}\n";
        std::fs::write(&file, src).unwrap();
        write_minimal_graph_with_def_and_mir(
            &tmp,
            "canon_mini_agent",
            "app::validate",
            &file,
            src,
            "validate",
        );
        let action = json!({
            "action": "execution_path",
            "crate": "canon_mini_agent",
            "from": "app::validate",
            "to": "app::validate",
            "rationale": "Persist a repair plan for the selected symbol."
        });

        let (_done, out) = handle_execution_path_action(&tmp, &action).unwrap();

        assert!(out.contains("Repair plan:"), "unexpected: {out}");
        let latest = tmp
            .join("state")
            .join("reports")
            .join("execution_path")
            .join("canon_mini_agent.latest.json");
        assert!(latest.exists(), "missing {}", latest.display());
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&latest).unwrap()).unwrap();
        assert_eq!(
            parsed.get("from").and_then(|v| v.as_str()),
            Some("app::validate")
        );
        assert_eq!(
            parsed
                .get("top_target")
                .and_then(|v| v.get("symbol"))
                .and_then(|v| v.as_str()),
            Some("app::validate")
        );
        assert!(parsed.get("apply_patch_template").is_some());
    }

    #[test]
    fn execution_path_applies_learning_bias_from_prior_success() {
        let tmp = fresh_test_dir("execution-path-learning-bias");
        let file = tmp.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let src = "fn validate() {}\n";
        std::fs::write(&file, src).unwrap();
        write_minimal_graph_with_def_and_mir(
            &tmp,
            "canon_mini_agent",
            "app::validate",
            &file,
            src,
            "validate",
        );
        let reports_dir = tmp.join("state").join("reports");
        std::fs::create_dir_all(&reports_dir).unwrap();
        std::fs::write(
            reports_dir.join("execution_learning.jsonl"),
            concat!(
                "{\"crate\":\"canon_mini_agent\",\"top_target\":{\"symbol\":\"app::validate\"},",
                "\"verification\":{\"verified\":true}}\n"
            ),
        )
        .unwrap();
        let action = json!({
            "action": "execution_path",
            "crate": "canon_mini_agent",
            "from": "app::validate",
            "to": "app::validate",
            "rationale": "Prefer symbols that succeeded on similar prior patches."
        });

        let (_done, out) = handle_execution_path_action(&tmp, &action).unwrap();

        assert!(out.contains("learned success x1"), "unexpected: {out}");
    }

    #[test]
    fn rename_symbol_renames_via_semantic_spans() {
        let tmp = fresh_test_dir("rename-symbol-success");
        let file = tmp.join("lib.rs");
        let src = "fn foo() {\n    foo();\n}\n";
        std::fs::write(&file, src).unwrap();
        write_minimal_graph_for_ident(&tmp, "canon_mini_agent", "foo", &file, src, "foo");
        let action = json!({
            "crate": "canon_mini_agent",
            "old_symbol": "foo",
            "new_symbol": "bar",
            "question": "rename foo to bar",
            "rationale": "Rename symbol",
            "predicted_next_actions": [
                {"action": "cargo_test", "intent": "verify"},
                {"action": "run_command", "intent": "check"}
            ]
        });

        let (_done, out) = handle_rename_symbol_action("solo", 1, &tmp, &action).unwrap();
        assert!(out.contains("rename_symbol ok"));
        let persisted = std::fs::read_to_string(&file).unwrap();
        assert!(persisted.contains("fn bar()"));
        assert!(persisted.contains("bar();"));
        assert!(!persisted.contains("foo"));
    }

    #[test]
    fn rename_symbol_rejects_span_mismatch_when_graph_is_stale() {
        let tmp = fresh_test_dir("rename-symbol-old-name-mismatch");
        let file = tmp.join("lib.rs");
        let src = "fn baz() {}\n";
        std::fs::write(&file, src).unwrap();
        // Graph claims `foo` spans exist at offsets where the file contains `baz`.
        write_minimal_graph_for_ident(&tmp, "canon_mini_agent", "foo", &file, src, "baz");
        let action = json!({
            "crate": "canon_mini_agent",
            "old_symbol": "foo",
            "new_symbol": "bar",
            "question": "rename foo to bar",
            "rationale": "Rename symbol",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "re-check position"},
                {"action": "message", "intent": "report blocker"}
            ]
        });

        let err = handle_rename_symbol_action("solo", 1, &tmp, &action)
            .unwrap_err()
            .to_string();
        assert!(err.contains("span mismatch"), "unexpected: {err}");
    }

    #[test]
    fn symbols_index_writes_deterministic_sorted_unique_output() {
        let tmp = fresh_test_dir("symbols-index-deterministic");
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(
            tmp.join("src/a.rs"),
            "pub struct Alpha {}\nimpl Alpha { pub fn new() -> Self { Self {} } }\n",
        )
        .unwrap();
        std::fs::write(tmp.join("src/b.rs"), "pub enum Beta { One }\n").unwrap();

        let action = json!({
            "path": "src",
            "out": "state/symbols.json",
            "rationale": "Index symbols",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "inspect symbols"},
                {"action": "rename_symbol", "intent": "rename a selected symbol"}
            ]
        });
        let (_done, out) = handle_symbols_index_action(&tmp, &action).unwrap();
        assert!(out.contains("symbols_index ok"));

        let symbols_path = tmp.join("state/symbols.json");
        assert!(symbols_path.exists());
        let raw = std::fs::read_to_string(&symbols_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.get("version").and_then(|v| v.as_u64()), Some(1));
        let symbols = parsed
            .get("symbols")
            .and_then(|v| v.as_array())
            .expect("symbols array");
        assert!(!symbols.is_empty());
        let mut prev: Option<(String, u64, u64, String, String)> = None;
        for sym in symbols {
            let file = sym.get("file").and_then(|v| v.as_str()).unwrap_or("");
            let start = sym
                .get("span")
                .and_then(|s| s.get("start"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let end = sym
                .get("span")
                .and_then(|s| s.get("end"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let kind = sym.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let name = sym.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let key = (
                file.to_string(),
                start,
                end,
                kind.to_string(),
                name.to_string(),
            );
            if let Some(prev_key) = prev.take() {
                assert!(
                    prev_key < key,
                    "symbols output should be strictly sorted and unique"
                );
            }
            prev = Some(key);
        }
    }

    #[test]
    fn rustc_actions_read_graph_json_when_present() {
        let tmp = fresh_test_dir("rustc-graph-actions");
        let file = tmp.join("lib.rs");
        let src = "fn foo() { foo(); }\n";
        std::fs::write(&file, src).unwrap();
        write_minimal_graph_with_def_and_mir(
            &tmp,
            "canon_mini_agent",
            "app::foo",
            &file,
            src,
            "foo",
        );

        let action = json!({
            "crate": "canon_mini_agent",
            "mode": "hir-tree",
            "symbol": "app::foo",
            "extra": ""
        });
        let (_done, out_hir) =
            super::handle_rustc_action("solo", 1, "rustc_hir", &tmp, &action).unwrap();
        assert!(
            out_hir.contains("rustc_hir ok (graph)"),
            "unexpected: {out_hir}"
        );
        assert!(out_hir.contains("fn foo"), "unexpected: {out_hir}");

        let action = json!({
            "crate": "canon_mini_agent",
            "mode": "mir",
            "extra": ""
        });
        let (_done, out_mir) =
            super::handle_rustc_action("solo", 1, "rustc_mir", &tmp, &action).unwrap();
        assert!(
            out_mir.contains("rustc_mir ok (graph)"),
            "unexpected: {out_mir}"
        );
        assert!(out_mir.contains("app::foo"), "unexpected: {out_mir}");
        assert!(out_mir.contains("fp1"), "unexpected: {out_mir}");
    }

    #[test]
    fn rustc_mir_supports_symbol_field_for_focused_summary() {
        let tmp = fresh_test_dir("rustc-mir-symbol");
        let file = tmp.join("lib.rs");
        let src = "fn foo() { foo(); }\n";
        std::fs::write(&file, src).unwrap();
        write_minimal_graph_with_def_and_mir(
            &tmp,
            "canon_mini_agent",
            "tools::handle_objectives_action",
            &file,
            src,
            "foo",
        );

        let action = json!({
            "crate": "canon_mini_agent",
            "mode": "mir",
            "symbol": "handle_objectives_action",
            "extra": ""
        });
        let (_done, out) =
            super::handle_rustc_action("solo", 1, "rustc_mir", &tmp, &action).unwrap();
        assert!(
            out.contains("symbol: handle_objectives_action"),
            "unexpected: {out}"
        );
        assert!(out.contains("rank_by_blocks:"), "unexpected: {out}");
        assert!(out.contains("fingerprint=fp1"), "unexpected: {out}");
    }

    #[test]
    fn symbols_rename_candidates_derives_heuristic_candidates() {
        let tmp = fresh_test_dir("symbols-rename-candidates");
        std::fs::create_dir_all(tmp.join("state")).unwrap();
        let symbols_json = serde_json::json!({
            "version": 1,
            "symbols": [
                {"name":"tmp","kind":"function","file":"src/a.rs","span":{"start":1,"end":4,"line":1,"column":1,"end_line":1,"end_column":4}},
                {"name":"get_data","kind":"function","file":"src/a.rs","span":{"start":10,"end":18,"line":2,"column":1,"end_line":2,"end_column":9}},
                {"name":"fetch_data","kind":"function","file":"src/b.rs","span":{"start":20,"end":30,"line":3,"column":1,"end_line":3,"end_column":11}},
                {"name":"clear_name","kind":"function","file":"src/c.rs","span":{"start":40,"end":50,"line":4,"column":1,"end_line":4,"end_column":11}}
            ]
        });
        std::fs::write(
            tmp.join("state/symbols.json"),
            serde_json::to_string_pretty(&symbols_json).unwrap(),
        )
        .unwrap();
        let action = json!({
            "symbols_path": "state/symbols.json",
            "out": "state/rename_candidates.json",
            "rationale": "derive candidates",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "inspect"},
                {"action": "rename_symbol", "intent": "apply"}
            ]
        });

        let (_done, out) = handle_symbols_rename_candidates_action(&tmp, &action).unwrap();
        assert!(out.contains("symbols_rename_candidates ok"));
        let raw = std::fs::read_to_string(tmp.join("state/rename_candidates.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let candidates = parsed
            .get("candidates")
            .and_then(|v| v.as_array())
            .expect("candidates array");
        assert!(!candidates.is_empty());
        assert!(candidates
            .iter()
            .any(|c| c.get("name").and_then(|v| v.as_str()) == Some("tmp")));
        assert!(candidates.iter().any(|c| {
            c.get("name").and_then(|v| v.as_str()) == Some("get_data")
                && c.get("reasons")
                    .and_then(|v| v.as_array())
                    .is_some_and(|arr| {
                        arr.iter().any(|r| {
                            r.as_str()
                                .unwrap_or("")
                                .contains("inconsistent verb prefix")
                        })
                    })
        }));
    }

    #[test]
    fn symbols_prepare_rename_writes_ready_action_payload() {
        let tmp = fresh_test_dir("symbols-prepare-rename");
        std::fs::create_dir_all(tmp.join("state")).unwrap();
        let candidates_json = serde_json::json!({
            "version": 1,
            "source_symbols_path": "state/symbols.json",
            "candidates": [
                {"name":"tmp","kind":"function","file":"src/a.rs","span":{"start":1,"end":4,"line":10,"column":5,"end_line":10,"end_column":8},"score":55,"reasons":["name is ambiguous/generic"]}
            ]
        });
        std::fs::write(
            tmp.join("state/rename_candidates.json"),
            serde_json::to_string_pretty(&candidates_json).unwrap(),
        )
        .unwrap();
        let action = json!({
            "candidates_path": "state/rename_candidates.json",
            "index": 0,
            "out": "state/next_rename_action.json",
            "rationale": "prepare rename action",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "inspect payload"},
                {"action": "rename_symbol", "intent": "execute"}
            ]
        });

        let (_done, out) = handle_symbols_prepare_rename_action(&tmp, &action).unwrap();
        assert!(out.contains("symbols_prepare_rename ok"));
        let raw = std::fs::read_to_string(tmp.join("state/next_rename_action.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.get("version").and_then(|v| v.as_u64()), Some(1));
        let rename_action = parsed.get("rename_action").expect("rename_action");
        assert_eq!(
            rename_action
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "rename_symbol"
        );
        // v2 payload shape: ensure symbol-based fields exist instead of span-based fields
        assert!(rename_action.get("old_symbol").is_some());
        assert!(rename_action.get("new_symbol").is_some());
        // ensure deprecated fields are not present
        assert!(rename_action.get("path").is_none());
        assert!(rename_action.get("line").is_none());
        assert!(rename_action.get("column").is_none());
    }

    #[test]
    fn plan_update_task_rejects_reopened_task_without_regression_linkage() {
        let tmp = fresh_test_dir("rejects-reopened-task-without-regression-linkage");
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Regression-linked task",
      "status": "done",
      "priority": 1,
      "steps": ["existing regression coverage"]
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "update_task",
            "task": {
                "id": "T1",
                "status": "in_progress",
                "steps": ["resume implementation without linked test"]
            },
            "rationale": "Exercise reopened-task enforcement"
        });

        let err = handle_plan_action("solo", &tmp, &action)
            .unwrap_err()
            .to_string();

        assert!(err.contains("reopened task T1 must include regression-test linkage"));
    }

    #[test]
    fn plan_update_task_allows_reopened_task_with_regression_linkage() {
        let tmp = fresh_test_dir("allows-reopened-task-with-regression-linkage");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Regression-linked task",
      "status": "done",
      "priority": 1,
      "steps": ["existing regression coverage"]
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "update_task",
            "task": {
                "id": "T1",
                "status": "in_progress",
                "steps": ["add regression test linkage before reopening"]
            },
            "rationale": "Exercise reopened-task allowance"
        });

        let (_done, out) = handle_plan_action("solo", &tmp, &action).unwrap();

        assert!(out.contains("plan ok"));
        let persisted = std::fs::read_to_string(tmp.join(MASTER_PLAN_FILE)).unwrap();
        assert!(persisted.contains("\"status\": \"in_progress\""));
        assert!(persisted.contains("add regression test linkage before reopening"));
    }

    #[test]
    fn plan_set_plan_status_rejects_done_when_any_task_is_incomplete() {
        let tmp = fresh_test_dir("rejects-plan-done-while-task-incomplete");
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Completed task",
      "status": "done",
      "priority": 1
    },
    {
      "id": "T2",
      "title": "Incomplete task",
      "status": "in_progress",
      "priority": 2
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "set_plan_status",
            "status": "done",
            "rationale": "Exercise plan/task convergence guard"
        });

        let err = handle_plan_action("solo", &tmp, &action)
            .unwrap_err()
            .to_string();

        assert!(err.contains("plan status cannot be set to done while tasks remain incomplete"));
        let persisted = std::fs::read_to_string(tmp.join(MASTER_PLAN_FILE)).unwrap();
        assert!(persisted.contains("\"status\": \"in_progress\""));
    }

    #[test]
    fn plan_set_task_status_marks_only_target_task_done() {
        let tmp = fresh_test_dir("set-task-status-only-target-task");
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Task one",
      "status": "in_progress",
      "priority": 1
    },
    {
      "id": "T2",
      "title": "Task two",
      "status": "todo",
      "priority": 2
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "set_task_status",
            "task_id": "T1",
            "status": "done",
            "rationale": "Close only one task"
        });

        let (_done, out) = handle_plan_action("solo", &tmp, &action).unwrap();
        assert!(out.contains("plan ok"));

        let persisted = std::fs::read_to_string(tmp.join(MASTER_PLAN_FILE)).unwrap();
        assert!(persisted.contains("\"id\": \"T1\""));
        assert!(persisted.contains("\"status\": \"done\""));
        assert!(persisted.contains("\"id\": \"T2\""));
        assert!(persisted.contains("\"status\": \"todo\""));
        assert!(persisted.contains("\"status\": \"in_progress\""));
    }

    #[test]
    fn plan_set_plan_status_allows_task_id_provenance_field() {
        let tmp = fresh_test_dir("set-plan-status-allows-task-id");
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [{"id":"T1","status":"todo"}],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "set_plan_status",
            "task_id": "T1",
            "status": "in_progress",
            "rationale": "Invalid mixed payload"
        });

        let (_, out) = handle_plan_action("solo", &tmp, &action)
            .expect("set_plan_status should accept provenance task_id");
        assert!(out.contains("plan ok"));
    }

    #[test]
    fn plan_set_task_status_rejects_task_object_field() {
        let tmp = fresh_test_dir("set-task-status-rejects-task-object");
        std::fs::write(
            tmp.join(MASTER_PLAN_FILE),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [{"id":"T1","status":"todo"}],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "set_task_status",
            "task_id": "T1",
            "status": "done",
            "task": {"id":"T1","status":"done"},
            "rationale": "Invalid mixed payload"
        });

        let err = handle_plan_action("solo", &tmp, &action)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not accept task"));
    }

    #[test]
    fn objectives_update_objective_auto_creates_missing_id() {
        let tmp = fresh_test_dir("objective-update-not-found-context");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    },
    {
      "id": "obj_beta",
      "title": "Beta",
      "status": "active",
      "scope": "beta scope",
      "authority_files": ["src/objectives.rs"],
      "category": "quality",
      "level": "low",
      "description": "beta",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "update_objective",
            "objective_id": "obj_missing",
            "updates": {
                "scope": "updated"
            }
        });

        let (_done, out) = handle_objectives_action(&tmp, &action).unwrap();
        assert!(out.contains("objectives update_objective ok (auto-created)"));
        let persisted =
            std::fs::read_to_string(tmp.join("agent_state").join("OBJECTIVES.json")).unwrap();
        assert!(persisted.contains("\"id\": \"obj_missing\""));
        assert!(persisted.contains("\"scope\": \"updated\""));
    }

    #[test]
    fn objectives_set_status_matches_normalized_id() {
        let tmp = fresh_test_dir("objective-set-status-normalized-id");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "set_status",
            "objective_id": "`obj_alpha`",
            "status": "done"
        });

        let (_done, out) = handle_objectives_action(&tmp, &action).unwrap();

        assert!(out.contains("objectives set_status ok"));
        let persisted =
            std::fs::read_to_string(tmp.join("agent_state").join("OBJECTIVES.json")).unwrap();
        assert!(persisted.contains("\"status\": \"done\""));
    }

    #[test]
    fn objectives_update_objective_auto_creates_with_normalized_id() {
        let tmp = fresh_test_dir("objective-update-raw-and-normalized-context");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "update_objective",
            "objective_id": "`obj_missing`",
            "updates": {
                "scope": "updated"
            }
        });

        let (_done, out) = handle_objectives_action(&tmp, &action).unwrap();
        assert!(out.contains("objectives update_objective ok (auto-created)"));
        let persisted =
            std::fs::read_to_string(tmp.join("agent_state").join("OBJECTIVES.json")).unwrap();
        assert!(persisted.contains("\"id\": \"obj_missing\""));
        assert!(persisted.contains("\"scope\": \"updated\""));
    }

    #[test]
    fn objectives_create_objective_reports_raw_and_normalized_duplicate_context() {
        let tmp = fresh_test_dir("objective-create-duplicate-context");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "create_objective",
            "objective": {
                "id": "`obj_alpha`",
                "title": "Alpha duplicate",
                "status": "active",
                "scope": "duplicate scope",
                "authority_files": ["src/tools.rs"],
                "category": "quality",
                "level": "low",
                "description": "duplicate",
                "requirement": [],
                "verification": [],
                "success_criteria": []
            }
        });

        let err = handle_objectives_action(&tmp, &action)
            .unwrap_err()
            .to_string();

        assert!(err.contains("objective id already exists:"));
        assert!(err.contains("requested_raw="));
        assert!(err.contains("requested_id=obj_alpha"));
        assert!(err.contains("compared_ids=[\"obj_alpha\"]"));
        assert!(err.contains("compared_normalized_ids=[\"obj_alpha\"]"));
    }

    #[test]
    fn objectives_create_update_read_lifecycle_succeeds() {
        let tmp = fresh_test_dir("objective-create-update-read-lifecycle");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let create_action = json!({
            "op": "create_objective",
            "objective": {
                "id": "obj_lifecycle",
                "title": "Lifecycle",
                "status": "active",
                "scope": "objective lifecycle coverage",
                "authority_files": ["src/tools.rs", "agent_state/OBJECTIVES.json"],
                "category": "quality",
                "level": "medium",
                "description": "create/update/read lifecycle objective",
                "requirement": ["create succeeds"],
                "verification": [],
                "success_criteria": ["updated objective is readable"]
            }
        });
        let (_done, create_out) = handle_objectives_action(&tmp, &create_action).unwrap();
        assert!(create_out.contains("objectives create_objective ok"));

        let update_action = json!({
            "op": "update_objective",
            "objective_id": "obj_lifecycle",
            "updates": {
                "scope": "updated lifecycle scope",
                "description": "updated lifecycle objective",
                "verification": ["updated through handle_objectives_action"]
            }
        });
        let (_done, update_out) = handle_objectives_action(&tmp, &update_action).unwrap();
        assert!(update_out.contains("objectives update_objective ok"));

        let read_action = json!({ "op": "read", "include_done": true });
        let (_done, read_out) = handle_objectives_action(&tmp, &read_action).unwrap();
        assert!(read_out.contains("\"id\": \"obj_lifecycle\""));
        assert!(read_out.contains("\"scope\": \"updated lifecycle scope\""));
        assert!(read_out.contains("\"description\": \"updated lifecycle objective\""));
        assert!(read_out.contains("updated through handle_objectives_action"));
    }

    #[test]
    fn objectives_replace_alias_writes_objectives_atomically() {
        let tmp = fresh_test_dir("objective-replace-alias");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "replace",
            "objectives": {
                "version": 1,
                "objectives": [
                    {
                        "id": "obj_runtime_authority",
                        "title": "Restore runtime objective authority",
                        "status": "active",
                        "scope": "repair runtime authority",
                        "authority_files": ["agent_state/OBJECTIVES.json"],
                        "category": "correctness",
                        "level": "high",
                        "description": "restored from alias replace",
                        "requirement": [],
                        "verification": [],
                        "success_criteria": []
                    }
                ],
                "goal": [],
                "instrumentation": [],
                "definition_of_done": [],
                "non_goals": []
            }
        });

        let (_done, out) = handle_objectives_action(&tmp, &action).unwrap();
        assert!(out.contains("objectives replace_objectives ok"));

        let persisted =
            std::fs::read_to_string(tmp.join("agent_state").join("OBJECTIVES.json")).unwrap();
        assert!(persisted.contains("obj_runtime_authority"));
        let parsed: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        assert_eq!(
            parsed["objectives"][0]["id"].as_str(),
            Some("obj_runtime_authority")
        );

        let tlog = std::fs::read_to_string(tmp.join("agent_state").join("tlog.ndjson")).unwrap();
        assert!(tlog.contains("workspace_artifact_write_requested"));
        assert!(tlog.contains("workspace_artifact_write_applied"));
        assert!(tlog.contains("agent_state/OBJECTIVES.json"));
    }

    #[test]
    fn objectives_update_objective_emits_attempt_and_success_trace_records() {
        let tmp = fresh_test_dir("objective-update-trace-records");
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        std::fs::write(
            tmp.join("agent_state").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let log_prefix = format!(
            "objective-update-trace-{}",
            fresh_test_dir("trace-log-prefix").display()
        );
        init_log_paths(&log_prefix);
        let action_log = crate::logging::current_action_log_path_for_tests()
            .expect("action log path after init");
        let before_count = std::fs::read_to_string(&action_log)
            .ok()
            .map(|raw| raw.lines().filter(|line| !line.trim().is_empty()).count())
            .unwrap_or(0);

        let action = json!({
            "op": "update_objective",
            "objective_id": "obj_alpha",
            "updates": {
                "scope": "updated alpha scope"
            }
        });

        let (_done, out) = handle_objectives_action(&tmp, &action).unwrap();
        assert!(out.contains("objectives update_objective ok"));

        let raw = std::fs::read_to_string(&action_log).expect("read action log after update");
        let records: Vec<Value> = raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let new_records = &records[before_count..];
        let objective_records: Vec<&Value> = new_records
            .iter()
            .filter(|record| {
                record.get("kind").and_then(|v| v.as_str()) == Some("orch")
                    && record.get("phase").and_then(|v| v.as_str())
                        == Some("objective_operation_context")
            })
            .collect();

        let matching_records: Vec<&Value> = objective_records
            .iter()
            .copied()
            .filter(|record| {
                let meta = record.get("meta");
                meta.and_then(|meta| meta.get("operation"))
                    .and_then(|v| v.as_str())
                    == Some("update_objective")
                    && meta
                        .and_then(|meta| meta.get("requested_id"))
                        .and_then(|v| v.as_str())
                        == Some("obj_alpha")
            })
            .collect();
        let attempt = matching_records
            .iter()
            .rev()
            .copied()
            .find(|record| {
                record
                    .get("meta")
                    .and_then(|meta| meta.get("outcome"))
                    .and_then(|v| v.as_str())
                    == Some("attempt")
            })
            .expect("latest attempt record");
        assert!(attempt.get("text").is_none());
        let attempt_meta = attempt.get("meta").expect("attempt meta");
        assert_eq!(
            attempt_meta.get("operation").and_then(|v| v.as_str()),
            Some("update_objective")
        );
        assert_eq!(
            attempt_meta.get("outcome").and_then(|v| v.as_str()),
            Some("attempt")
        );
        assert_eq!(
            attempt_meta.get("requested_raw").and_then(|v| v.as_str()),
            Some("obj_alpha")
        );
        assert_eq!(
            attempt_meta.get("requested_id").and_then(|v| v.as_str()),
            Some("obj_alpha")
        );
        assert_eq!(
            attempt_meta.get("compared_ids"),
            Some(&json!(["obj_alpha"]))
        );
        assert_eq!(
            attempt_meta.get("compared_normalized_ids"),
            Some(&json!(["obj_alpha"]))
        );

        let success = matching_records
            .iter()
            .rev()
            .copied()
            .find(|record| {
                record
                    .get("meta")
                    .and_then(|meta| meta.get("outcome"))
                    .and_then(|v| v.as_str())
                    == Some("success")
            })
            .expect("latest success record");
        assert!(success.get("text").is_none());
        let success_meta = success.get("meta").expect("success meta");
        assert_eq!(
            success_meta.get("operation").and_then(|v| v.as_str()),
            Some("update_objective")
        );
        assert_eq!(
            success_meta.get("outcome").and_then(|v| v.as_str()),
            Some("success")
        );
        assert_eq!(
            success_meta.get("requested_raw").and_then(|v| v.as_str()),
            Some("obj_alpha")
        );
        assert_eq!(
            success_meta.get("requested_id").and_then(|v| v.as_str()),
            Some("obj_alpha")
        );
        assert_eq!(
            success_meta.get("compared_ids"),
            Some(&json!(["obj_alpha"]))
        );
        assert_eq!(
            success_meta.get("compared_normalized_ids"),
            Some(&json!(["obj_alpha"]))
        );

        let persisted =
            std::fs::read_to_string(tmp.join("agent_state").join("OBJECTIVES.json")).unwrap();
        assert!(persisted.contains("\"scope\": \"updated alpha scope\""));

        let last_objective_record = objective_records
            .last()
            .copied()
            .expect("at least one objective trace record");
        assert_eq!(
            last_objective_record.get("phase").and_then(|v| v.as_str()),
            Some("objective_operation_context")
        );
    }
}
