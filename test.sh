cd /workspace/ai_sandbox/canon-mini-agent

cat <<'PATCH' | /opt/apply_patch/bin/apply_patch
*** Begin Patch
*** Update File: src/events.rs
@@
     GitCheckpointBlocked {
         reason: String,
         risk: String,
         verification_requested: bool,
         rust_sensitive_changes: bool,
         changed_paths: Vec<String>,
         required_gate: String,
         signature: String,
     },
+    SupervisorRestartRequested {
+        reason: String,
+        mode: String,
+        current_binary_path: String,
+        current_binary_mtime_ms: u64,
+        next_binary_path: String,
+        next_binary_mtime_ms: u64,
+        verification_requested: bool,
+        pending_defer_checks: u32,
+        signature: String,
+    },
     CheckpointSaved {
         phase: String,
     },
     CheckpointLoaded {
         phase: String,
     },
+    SupervisorChildStarted {
+        binary_path: String,
+        build_kind: String,
+        pid: u32,
+        binary_mtime_ms: u64,
+        signature: String,
+    },
*** Update File: src/supervisor.rs
@@
 fn preferred_build_kind(prefer_release: bool) -> BuildKind {
     if prefer_release {
         BuildKind::Release
     } else {
         BuildKind::Debug
     }
 }
+
+fn build_kind_label(kind: BuildKind) -> &'static str {
+    match kind {
+        BuildKind::Debug => "debug",
+        BuildKind::Release => "release",
+    }
+}
+
+fn system_time_ms(ts: SystemTime) -> u64 {
+    ts.duration_since(UNIX_EPOCH)
+        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
+        .unwrap_or_default()
+}
+
+fn record_supervisor_restart_requested(
+    root: &Path,
+    state_dir: &Path,
+    reason: &str,
+    mode: &str,
+    current: &BinaryCandidate,
+    next: Option<&BinaryCandidate>,
+    pending_defer_checks: u32,
+) {
+    let next = next.unwrap_or(current);
+    let verification_requested = rust_patch_verification_requested(state_dir);
+    let current_binary_path = current.path.display().to_string();
+    let next_binary_path = next.path.display().to_string();
+    let current_binary_mtime_ms = system_time_ms(current.mtime);
+    let next_binary_mtime_ms = system_time_ms(next.mtime);
+    let signature = artifact_write_signature(&[
+        "supervisor_restart_requested",
+        reason,
+        mode,
+        &current_binary_path,
+        &current_binary_mtime_ms.to_string(),
+        &next_binary_path,
+        &next_binary_mtime_ms.to_string(),
+        &verification_requested.to_string(),
+        &pending_defer_checks.to_string(),
+    ]);
+    let effect = EffectEvent::SupervisorRestartRequested {
+        reason: reason.to_string(),
+        mode: mode.to_string(),
+        current_binary_path,
+        current_binary_mtime_ms,
+        next_binary_path,
+        next_binary_mtime_ms,
+        verification_requested,
+        pending_defer_checks,
+        signature,
+    };
+    if let Err(err) = record_effect_for_workspace(root, effect) {
+        eprintln!("[canon-mini-supervisor] supervisor restart tlog effect failed: {err:#}");
+    }
+}
+
+fn record_supervisor_child_started(root: &Path, current: &BinaryCandidate, pid: u32) {
+    let binary_path = current.path.display().to_string();
+    let build_kind = build_kind_label(current.kind).to_string();
+    let binary_mtime_ms = system_time_ms(current.mtime);
+    let signature = artifact_write_signature(&[
+        "supervisor_child_started",
+        &binary_path,
+        &build_kind,
+        &pid.to_string(),
+        &binary_mtime_ms.to_string(),
+    ]);
+    let effect = EffectEvent::SupervisorChildStarted {
+        binary_path,
+        build_kind,
+        pid,
+        binary_mtime_ms,
+        signature,
+    };
+    if let Err(err) = record_effect_for_workspace(root, effect) {
+        eprintln!("[canon-mini-supervisor] supervisor child-start tlog effect failed: {err:#}");
+    }
+}
@@
-            start_supervisor_child(&current, filtered_args, child_pid)?;
+            start_supervisor_child(root, &current, filtered_args, child_pid)?;
@@
         child.try_wait().context("wait child")?,
         root,
         state_dir,
+        current,
         prefer_release,
     ) {
@@
 fn handle_child_exit_status(
     status: Option<ExitStatus>,
     root: &Path,
     state_dir: &Path,
+    current: &BinaryCandidate,
     prefer_release: bool,
 ) -> bool {
@@
     }
     eprintln!("[canon-mini-supervisor] restarting due to failure...");
+    record_supervisor_restart_requested(
+        root,
+        state_dir,
+        "failure-restart",
+        "failure",
+        current,
+        None,
+        0,
+    );
     stage_commit_push_before_restart(root, state_dir, "failure-restart", prefer_release);
@@
 fn start_supervisor_child(
+    root: &Path,
     current: &BinaryCandidate,
     filtered_args: &[String],
     child_pid: &Arc<AtomicU32>,
 ) -> Result<(Child, Option<BinaryCandidate>, SystemTime)> {
     let child = spawn_child(current, filtered_args)?;
     child_pid.store(child.id(), Ordering::SeqCst);
+    record_supervisor_child_started(root, current, child.id());
@@
     maybe_restart_for_pending_update(
         root,
         state_dir,
+        current,
         pending_update.as_ref(),
@@
 fn maybe_restart_for_pending_update(
     root: &Path,
     state_dir: &Path,
+    current: &BinaryCandidate,
     pending_update: Option<&BinaryCandidate>,
@@
-        stage_commit_push_before_restart(root, state_dir, "single-role-update", prefer_release);
+        record_supervisor_restart_requested(root, state_dir, "single-role-update", "single-role", current, Some(updated), *pending_update_defer_checks);
+        stage_commit_push_before_restart(root, state_dir, "single-role-update", prefer_release);
@@
-        stage_commit_push_before_restart(
+        record_supervisor_restart_requested(root, state_dir, "orchestrate-idle-update", "orchestrate", current, Some(updated), *pending_update_defer_checks);
+        stage_commit_push_before_restart(
@@
-        stage_commit_push_before_restart(
+        record_supervisor_restart_requested(root, state_dir, "orchestrate-deferred-update-timeout", "orchestrate", current, Some(updated), *pending_update_defer_checks);
+        stage_commit_push_before_restart(
*** Update File: src/evaluation.rs
@@
     pub artifact_write_applies: usize,
     pub unapplied_artifact_writes: usize,
     pub git_checkpoint_blocked: usize,
     pub unsafe_checkpoint_attempts: usize,
+    pub supervisor_restart_requests: usize,
+    pub supervisor_child_starts: usize,
+    pub restart_requests_without_child_start: usize,
     pub score: f64,
 }
@@
     let mut actionable_lag_by_next_kind: HashMap<String, u64> = HashMap::new();
     let mut payload_bytes_by_kind: HashMap<String, u64> = HashMap::new();
+    let mut unmatched_restart_requests = 0usize;
@@
             Event::Effect {
                 event:
                     EffectEvent::GitCheckpointBlocked {
@@
                     signals.unsafe_checkpoint_attempts += 1;
                 }
             }
+            Event::Effect {
+                event: EffectEvent::SupervisorRestartRequested { .. },
+            } => {
+                signals.supervisor_restart_requests += 1;
+                unmatched_restart_requests = unmatched_restart_requests.saturating_add(1);
+            }
+            Event::Effect {
+                event: EffectEvent::SupervisorChildStarted { .. },
+            } => {
+                signals.supervisor_child_starts += 1;
+                unmatched_restart_requests = unmatched_restart_requests.saturating_sub(1);
+            }
             _ => {}
         }
     }
@@
     signals.unapplied_artifact_writes = requested_artifact_signatures
         .difference(&applied_artifact_signatures)
         .count();
+    signals.restart_requests_without_child_start = unmatched_restart_requests;
@@
     let checkpoint_score =
         1.0 - safe_ratio(signals.unsafe_checkpoint_attempts as f64, 4.0).min(0.75);
+    let restart_score = if signals.supervisor_restart_requests == 0 {
+        1.0
+    } else {
+        1.0 - safe_ratio(
+            signals.restart_requests_without_child_start as f64,
+            signals.supervisor_restart_requests as f64,
+        )
+        .min(0.75)
+    };
@@
         error_score,
         lag_score,
         checkpoint_score,
+        restart_score,
     ])
 }
*** Update File: src/complexity.rs
@@
     eval_report.insert(
         "last_executor_diff_payload_bytes".into(),
         json!(eval.tlog_delta_signals.last_executor_diff_payload_bytes),
     );
+    eval_report.insert(
+        "supervisor_restart_requests".into(),
+        json!(eval.tlog_delta_signals.supervisor_restart_requests),
+    );
+    eval_report.insert(
+        "supervisor_child_starts".into(),
+        json!(eval.tlog_delta_signals.supervisor_child_starts),
+    );
+    eval_report.insert(
+        "restart_requests_without_child_start".into(),
+        json!(eval.tlog_delta_signals.restart_requests_without_child_start),
+    );
     eval_report.insert("objectives".into(), json!(objectives_progress));
*** Update File: src/prompts.rs
@@
 C_allowed = no_rust_change ∨ (cargo_check_ok ∧ cargo_test_ok ∧ cargo_build_ok)\n\
+reload_proven = SupervisorRestartRequested ∧ SupervisorChildStarted(binary_path, mtime)\n\
 Order: observe truth → eval → plan ready work → execute bounded patch → verify gates → regenerate projections → append tlog effects → learn → gated commit.";
*** Update File: CANONICAL_PIPELINE.md
@@
 L = learn(failure_event) only when it changes invariant/eval/test/prompt behavior
 C_allowed = no_rust_change ∨ (cargo_check_ok ∧ cargo_test_ok ∧ cargo_build_ok)
+reload_proven = SupervisorRestartRequested ∧ SupervisorChildStarted(binary_path, mtime)
```

*** End Patch
PATCH

python - <<'PY'
import json
from pathlib import Path

p = Path("INVARIANTS.json")
data = json.loads(p.read_text())
items = data["invariants"] if isinstance(data, dict) and "invariants" in data else data

new = {
"id": "I21-supervisor-reload-proof",
"title": "Supervisor Restart/Reload Proof",
"category": "observability",
"level": "critical",
"error_class": "VerificationFailed",
"violation_action_kind": "supervisor_reload_proof",
"description": "Every supervisor restart or binary reload boundary must be represented in tlog by SupervisorRestartRequested followed by SupervisorChildStarted for the child binary that actually started.",
"clauses": [
"SupervisorRestartRequested records reason, mode, current binary identity, next binary identity, verification request state, and defer count",
"SupervisorChildStarted records binary path, build kind, pid, and binary mtime",
"Eval treats restart_requested without later child_started as reload_unproven and lowers canonical_delta_health",
"Plain stderr text such as restarting or started pid is not authoritative evidence"
],
}

if not any(isinstance(x, dict) and x.get("id") == new["id"] for x in items):
items.append(new)

p.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
PY

cargo check

