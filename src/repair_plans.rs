/// Repair plan registry — structured, planner-readable action blocks.
///
/// ## Closed loop
///
///   Eval → build_all_active_plans → RepairPlan { machine_verify }
///   → render into EVAL HEADER (planner reads)
///   → planner creates task
///   → executor patches
///   → eval_driver runs machine_verify.check() after next eval
///   → passed → PlanVerifyRecorded(passed=true) → task can be closed
///   → failed → PlanVerifyRecorded(passed=false) → failure count rises
///   → failure_count >= threshold → escalate
///
/// ## Plan kinds
///
///   eval_metric   — score below target for a measured eval dimension
///   invariant     — promoted invariant waiting for enforce/collapse decision
///   blocker_class — recurring error class with no enforced invariant
use std::path::Path;

use serde_json::{json, Map, Value};

// ── VerifySpec ────────────────────────────────────────────────────────────────

/// Machine-checkable closure condition for a repair plan.
/// Evaluated by `eval_driver` after each eval cycle to determine whether the
/// plan's work is done.
#[derive(Debug, Clone)]
pub enum VerifySpec {
    /// Score for `metric` in next eval is >= threshold.
    ScoreAbove { metric: &'static str, threshold: f64 },
    /// Score for `metric` strictly improved since the plan was created.
    ScoreImproves { metric: &'static str, from: f64 },
    /// String field `key` in eval map does not equal `value`.
    FieldNotEquals { key: &'static str, value: String },
    /// Integer field `key` in eval map equals `value`.
    FieldEqualsU64 { key: &'static str, value: u64 },
    /// The named invariant has status=enforced or status=collapsed.
    InvariantResolved { id: String },
    /// All sub-specs must pass.
    All(Vec<VerifySpec>),
}

impl VerifySpec {
    /// Evaluate this spec against the current eval JSON map and invariant text.
    pub fn check(&self, eval: &Map<String, Value>, invariant_text: &str) -> bool {
        match self {
            Self::ScoreAbove { metric, threshold } => {
                eval.get(*metric).and_then(|v| v.as_f64()).unwrap_or(0.0) >= *threshold
            }
            Self::ScoreImproves { metric, from } => {
                eval.get(*metric).and_then(|v| v.as_f64()).unwrap_or(0.0) > *from + 0.001
            }
            Self::FieldNotEquals { key, value } => {
                eval.get(*key).and_then(|v| v.as_str()).unwrap_or("") != value.as_str()
            }
            Self::FieldEqualsU64 { key, value } => {
                eval.get(*key).and_then(|v| v.as_u64()).unwrap_or(u64::MAX) == *value
            }
            Self::InvariantResolved { id } => invariant_resolved(invariant_text, id),
            Self::All(specs) => specs.iter().all(|s| s.check(eval, invariant_text)),
        }
    }

    /// Human-readable description used in the rendered REPAIR_PLAN block.
    pub fn description(&self) -> String {
        match self {
            Self::ScoreAbove { metric, threshold } => format_metric_threshold(metric, ">=", *threshold),
            Self::ScoreImproves { metric, from } => format_metric_threshold(metric, ">", *from),
            Self::FieldNotEquals { key, value } => format_field_not_equals(key, value),
            Self::FieldEqualsU64 { key, value } => format_field_equals_u64(key, *value),
            Self::InvariantResolved { id } => format_invariant_resolved_description(id),
            Self::All(specs) => specs
                .iter()
                .map(|s| s.description())
                .collect::<Vec<_>>()
                .join(" AND "),
        }
    }
}

fn format_metric_threshold(metric: &str, op: &str, value: f64) -> String {
    format!("{metric} {op} {value:.3}")
}

fn format_field_not_equals(key: &str, value: &str) -> String {
    format!("{key} != \"{value}\"")
}

fn format_field_equals_u64(key: &str, value: u64) -> String {
    format!("{key} == {value}")
}

fn format_invariant_resolved_description(id: &str) -> String {
    format!("{id} status in {{enforced, collapsed}}")
}

fn invariant_resolved(text: &str, id: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct F {
        #[serde(default)]
        invariants: Vec<E>,
    }
    #[derive(serde::Deserialize)]
    struct E {
        id: String,
        status: String,
    }
    let Ok(f) = serde_json::from_str::<F>(text) else {
        return false;
    };
    f.invariants
        .iter()
        .filter(|e| e.id == id)
        .any(|e| e.status == "enforced" || e.status == "collapsed")
}

// ── Core type ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RepairPlan {
    pub kind: &'static str,
    /// Stable identifier across eval cycles.
    /// Format: "eval_metric:{name}" | "invariant:{INV-xxx}" | "blocker_class:{key}"
    pub id: String,
    pub goal: String,
    pub trigger: String,
    pub policy: &'static str,
    /// Canonical mutation class the PLAN task must carry. This blocks
    /// heuristic rewrites from satisfying repeated-failure repair plans.
    pub required_mutation: String,
    /// Canonical target files the PLAN task must carry when the repair has a
    /// deterministic implementation surface.
    pub target_files: Vec<String>,
    pub action: String,
    /// Deterministic PLAN mutation the planner must apply for this plan.
    pub plan_mutation_template: String,
    /// Policy/persistence condition that prevents local-only fixes.
    pub persisted_policy: String,
    pub verify: String,
    /// Machine-checkable form of `verify` — evaluated by eval_driver each cycle.
    pub machine_verify: VerifySpec,
    pub owner: &'static str,
    pub evidence: &'static str,
    /// Lower = higher priority when sorting across plan kinds.
    pub priority: u8,
    /// Raw score for eval_metric plans; 0.0 for other kinds.
    pub score: f64,
}

// ── Rendering ─────────────────────────────────────────────────────────────────

pub fn render_plan(plan: &RepairPlan) -> String {
    format!(
        "REPAIR_PLAN\n\
        kind: {kind}\n\
        id: {id}\n\
        goal: {goal}\n\
        trigger: {trigger}\n\
        policy: {policy}\n\
        required_mutation: {required_mutation}\n\
        target_files: {target_files}\n\
        action: {action}\n\
        plan_mutation_template: {plan_mutation_template}\n\
        persisted_policy: {persisted_policy}\n\
        verify: {verify}\n\
        machine_verify: {machine_verify}\n\
        owner: {owner}\n\
        evidence: {evidence}",
        kind = plan.kind,
        id = plan.id,
        goal = plan.goal,
        trigger = plan.trigger,
        policy = plan.policy,
        required_mutation = plan.required_mutation,
        target_files = plan.target_files.join(", "),
        action = plan.action,
        plan_mutation_template = plan.plan_mutation_template,
        persisted_policy = plan.persisted_policy,
        verify = plan.verify,
        machine_verify = plan.machine_verify.description(),
        owner = plan.owner,
        evidence = plan.evidence,
    )
}

pub fn render_active_plans(plans: &[RepairPlan]) -> String {
    if plans.is_empty() {
        return String::new();
    }
    let blocks: Vec<String> = plans.iter().map(render_plan).collect();
    format!(
        "\nACTIVE REPAIR PLAN CONTRACT:\n\
        Each REPAIR_PLAN is binding. The planner must create/update an open PLAN task \
        with exact repair_plan_id, required_mutation, and all target_files copied from \
        the rendered plan. Heuristic-equivalent tasks do not satisfy this contract; \
        missing or mismatched bindings fail plan preflight/eval as planner repair drift.\n\
        {}\n",
        blocks.join("\n\n")
    )
}

// ── PLAN binding verifier ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanBindingVerification {
    pub passed: bool,
    pub description: String,
}

/// Verify that a repair plan is represented by an open PLAN task with the same
/// stable repair_plan_id, required_mutation, and target_files.
///
/// This is intentionally stricter than text matching. The convergence rule is:
/// same failure class → same repair_plan_id → same mutation template.
pub fn verify_plan_binding(plan: &RepairPlan, plan_text: &str) -> PlanBindingVerification {
    let plan_json: Value = match serde_json::from_str(plan_text) {
        Ok(value) => value,
        Err(err) => {
            return PlanBindingVerification {
                passed: false,
                description: format!("PLAN unreadable for {}: {err}", plan.id),
            };
        }
    };
    let Some(tasks) = plan_json.get("tasks").and_then(|v| v.as_array()) else {
        return PlanBindingVerification {
            passed: false,
            description: format!("PLAN missing tasks array for {}", plan.id),
        };
    };

    let open_statuses = ["ready", "in_progress", "needs_planning"];
    let exact_tasks: Vec<&Value> = tasks
        .iter()
        .filter(|task| {
            task.get("repair_plan_id")
                .and_then(|v| v.as_str())
                .map(|id| id == plan.id)
                .unwrap_or(false)
        })
        .filter(|task| {
            task.get("status")
                .and_then(|v| v.as_str())
                .map(|status| {
                    open_statuses
                        .iter()
                        .any(|allowed| status.eq_ignore_ascii_case(allowed))
                })
                .unwrap_or(false)
        })
        .collect();

    let Some(task) = exact_tasks.first() else {
        return PlanBindingVerification {
            passed: false,
            description: format!("PLAN missing open task with repair_plan_id={}", plan.id),
        };
    };

    let mut missing = Vec::new();
    let actual_mutation = task
        .get("required_mutation")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if actual_mutation != plan.required_mutation {
        missing.push(format!(
            "required_mutation={} (got {})",
            plan.required_mutation,
            if actual_mutation.is_empty() {
                "<missing>"
            } else {
                actual_mutation
            }
        ));
    }

    let actual_files: std::collections::BTreeSet<String> = task
        .get("target_files")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .map(|s| s.to_string())
        .collect();
    for required_file in &plan.target_files {
        if !actual_files.contains(required_file) {
            missing.push(format!("target_files contains {required_file}"));
        }
    }

    if missing.is_empty() {
        PlanBindingVerification {
            passed: true,
            description: format!(
                "PLAN binding ok: repair_plan_id={} required_mutation={} target_files=[{}]",
                plan.id,
                plan.required_mutation,
                plan.target_files.join(",")
            ),
        }
    } else {
        PlanBindingVerification {
            passed: false,
            description: format!(
                "PLAN binding failed for {}: missing {}",
                plan.id,
                missing.join("; ")
            ),
        }
    }
}

// ── Top-level builder ─────────────────────────────────────────────────────────

pub fn build_all_active_plans(
    eval: &Map<String, Value>,
    workspace: &Path,
    max_count: usize,
) -> Vec<RepairPlan> {
    let invariant_text = std::fs::read_to_string(
        workspace.join("agent_state").join("enforced_invariants.json"),
    )
    .unwrap_or_default();
    let blockers_text =
        std::fs::read_to_string(workspace.join("agent_state").join("blockers.json"))
            .unwrap_or_default();

    let mut plans: Vec<RepairPlan> = Vec::new();
    plans.extend(build_invariant_plans(&invariant_text, max_count));
    plans.extend(build_blocker_class_plans(&blockers_text, &invariant_text, max_count));
    plans.extend(build_eval_metric_plans(eval, max_count));

    plans.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.id.cmp(&b.id))
    });
    plans.truncate(max_count);
    plans
}

// Compatibility shim — used by prompt_inputs.rs.
pub fn render_weak_blocks(eval: &Map<String, Value>, max_count: usize) -> String {
    render_active_plans(&build_eval_metric_plans(eval, max_count))
}

// ── 1. Invariant lifecycle plans ──────────────────────────────────────────────

pub fn build_invariant_plans(invariant_text: &str, max: usize) -> Vec<RepairPlan> {
    #[derive(serde::Deserialize)]
    struct InvFile {
        #[serde(default)]
        invariants: Vec<InvEntry>,
    }
    #[derive(serde::Deserialize)]
    struct InvEntry {
        id: String,
        predicate_text: String,
        status: String,
        support_count: u64,
        #[serde(default)]
        gates: Vec<String>,
    }

    let Ok(file) = serde_json::from_str::<InvFile>(invariant_text) else {
        return Vec::new();
    };

    let mut plans: Vec<RepairPlan> = file
        .invariants
        .iter()
        .filter(|inv| inv.status == "promoted")
        .map(|inv| {
            let gates = inv.gates.join(", ");
            let inv_id = inv.id.clone();
            RepairPlan {
                kind: "invariant",
                id: format!("invariant:{}", inv.id),
                goal: "invariant lifecycle resolved — predicate enforced or collapsed".to_string(),
                trigger: format!(
                    "{id} promoted (support={support}, gates=[{gates}]): '{predicate}'",
                    id = inv.id,
                    support = inv.support_count,
                    predicate = truncate(&inv.predicate_text, 80),
                ),
                policy: "invariant_lifecycle",
                required_mutation: "invariant_lifecycle".to_string(),
                target_files: vec!["agent_state/enforced_invariants.json".to_string()],
                action: format!(
                    "evaluate predicate validity; then: invariants(op=enforce, id={id}) if \
                    valid, invariants(op=collapse, id={id}) if root cause is gone",
                    id = inv.id,
                ),
                plan_mutation_template: format!(
                    "plan(op=create_task|update_task, repair_plan_id=\"invariant:{id}\", \
                    required_mutation=\"invariant_lifecycle\", \
                    target_files=[\"agent_state/enforced_invariants.json\"], \
                    status=\"ready\", title=\"Resolve invariant {id}\", \
                    description=\"Enforce or collapse invariant {id} with evidence\")",
                    id = inv.id,
                ),
                persisted_policy: format!(
                    "default_behavior: invariant:{id} remains active until machine_verify \
                    passes; persist by enforcing or collapsing the invariant",
                    id = inv.id,
                ),
                verify: format!(
                    "{id} status=enforced OR status=collapsed in enforced_invariants.json",
                    id = inv.id,
                ),
                machine_verify: VerifySpec::InvariantResolved { id: inv_id },
                owner: "planner",
                evidence: "agent_state/enforced_invariants.json",
                priority: 10,
                score: 0.0,
            }
        })
        .collect();

    plans.sort_by(|a, b| a.id.cmp(&b.id));
    plans.truncate(max);
    plans
}

// ── 2. Blocker class plans ────────────────────────────────────────────────────

pub fn build_blocker_class_plans(
    blockers_text: &str,
    invariant_text: &str,
    max: usize,
) -> Vec<RepairPlan> {
    #[derive(serde::Deserialize)]
    struct BlockersFile {
        #[serde(default)]
        blockers: Vec<BlockerEntry>,
    }
    #[derive(serde::Deserialize)]
    struct BlockerEntry {
        error_class: String,
    }

    let Ok(file) = serde_json::from_str::<BlockersFile>(blockers_text) else {
        return Vec::new();
    };

    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for b in &file.blockers {
        if b.error_class == "unknown" {
            continue;
        }
        *counts.entry(b.error_class.clone()).or_default() += 1;
    }

    let invariant_lower = invariant_text.to_ascii_lowercase();

    let mut plans: Vec<RepairPlan> = counts
        .iter()
        .filter(|(key, _)| !invariant_lower.contains(key.as_str()))
        .map(|(key, count)| {
            let class_key = key.clone();
            let binding = crate::recovery::canonical_repair_binding_for_key(key);
            RepairPlan {
                kind: "blocker_class",
                id: binding.repair_plan_id.clone(),
                goal: format!(
                    "'{key}' error class covered by an enforced invariant so future \
                    occurrences are gated and tracked"
                ),
                trigger: format!(
                    "{count} occurrences of '{key}' with no matching invariant"
                ),
                policy: "synthesize_blocker_invariant",
                required_mutation: "synthesize_blocker_invariant".to_string(),
                target_files: vec!["src/invariant_discovery.rs".to_string()],
                action: format!(
                    "apply canonical mutation first: {template}; then patch \
                    src/invariant_discovery.rs — add detection rule for '{key}' \
                    emitting a typed invariant when support_count >= 3",
                    template = binding.plan_mutation_template,
                ),
                plan_mutation_template: binding.plan_mutation_template,
                persisted_policy: binding.persisted_policy,
                verify: format!(
                    "enforced_invariants.json contains '{key}' state_condition AND \
                    blocker_class_coverage improves on next eval"
                ),
                machine_verify: VerifySpec::FieldNotEquals {
                    key: "blocker_top_uncovered",
                    value: class_key,
                },
                owner: "executor",
                evidence: "agent_state/blockers.json, agent_state/enforced_invariants.json, \
                    src/invariant_discovery.rs",
                priority: 20,
                score: 0.0,
            }
        })
        .collect();

    plans.sort_by(|a, b| {
        let ca = counts
            .get(a.id.trim_start_matches("blocker_class:"))
            .copied()
            .unwrap_or(0);
        let cb = counts
            .get(b.id.trim_start_matches("blocker_class:"))
            .copied()
            .unwrap_or(0);
        cb.cmp(&ca).then(a.id.cmp(&b.id))
    });
    plans.truncate(max);
    plans
}

// ── 3. Eval metric plans ──────────────────────────────────────────────────────

fn status_label(score: f64, target: f64) -> &'static str {
    if score >= target {
        "pass"
    } else if score >= target * 0.70 {
        "weak"
    } else {
        "blocked"
    }
}

fn push_eval_metric_plan(
    plans: &mut Vec<RepairPlan>,
    metric: &'static str,
    score: f64,
    target: f64,
    goal: String,
    trigger: String,
    policy: &'static str,
    action: String,
    verify: String,
    machine_verify: VerifySpec,
    owner: &'static str,
    evidence: &'static str,
) {
    let status = status_label(score, target);
    if status == "pass" {
        return;
    }

    let repair_plan_id = format!("eval_metric:{metric}");
    plans.push(RepairPlan {
        kind: "eval_metric",
        id: repair_plan_id.clone(),
        goal,
        trigger,
        policy,
        required_mutation: policy.to_string(),
        target_files: Vec::new(),
        action,
        plan_mutation_template: format!(
            "plan(op=create_task|update_task, repair_plan_id=\"{repair_plan_id}\", \
            required_mutation=\"{policy}\", \
            status=\"ready\", title=\"Improve eval metric {metric}\", \
            description=\"Execute the rendered REPAIR_PLAN action and keep \
            this repair_plan_id bound until machine_verify passes\")"
        ),
        persisted_policy: format!(
            "default_behavior: eval_metric:{metric} remains active until \
            machine_verify passes; PLAN task must keep repair_plan_id={repair_plan_id}"
        ),
        verify,
        machine_verify,
        owner,
        evidence,
        priority: if status == "blocked" { 30 } else { 50 },
        score,
    });
}

struct EvalMetricValues<'a> {
    eval: &'a Map<String, Value>,
}

impl<'a> EvalMetricValues<'a> {
    fn new(eval: &'a Map<String, Value>) -> Self {
        Self { eval }
    }

    fn f64(&self, key: &str) -> f64 {
        self.f64_or(key, 0.0)
    }

    fn f64_or(&self, key: &str, default: f64) -> f64 {
        self.eval.get(key).and_then(|v| v.as_f64()).unwrap_or(default)
    }

    fn u64(&self, key: &str) -> u64 {
        self.eval.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
    }

    fn string(&self, key: &str) -> String {
        self.eval.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
    }

    fn arr_str(&self, key: &str) -> String {
        eval_array_string(self.eval, key)
    }
}

pub fn build_eval_metric_plans(eval: &Map<String, Value>, max: usize) -> Vec<RepairPlan> {
    let eval_values = EvalMetricValues::new(eval);

    let mut plans: Vec<RepairPlan> = Vec::new();

    macro_rules! plan {
        (
            metric: $metric:literal,
            target: $target:expr,
            score_key: $score_key:expr,
            score_default: $score_default:expr,
            goal: $goal:expr,
            trigger: $trigger:expr,
            policy: $policy:expr,
            action: $action:expr,
            verify: $verify:expr,
            machine_verify: $machine_verify:expr,
            owner: $owner:expr,
            evidence: $evidence:expr,
        ) => {{
            let score: f64 = eval_values.f64_or($score_key, $score_default);
            let target: f64 = $target;
            push_eval_metric_plan(
                &mut plans,
                $metric,
                score,
                target,
                $goal,
                $trigger,
                $policy,
                $action,
                $verify,
                $machine_verify,
                $owner,
                $evidence,
            );
        }};
    }

    plan!(
        metric: "objective_progress",
        target: 1.0,
        score_key: "objective_progress",
        score_default: 0.0,
        goal: "all objectives in OBJECTIVES.json marked done".to_string(),
        trigger: format!(
            "objective_progress={:.3}; one or more objectives not complete",
            eval_values.f64("objective_progress")
        ),
        policy: "close_or_update_objectives",
        action: "use objectives action (op: update_objective) to close complete objectives; \
            create objectives for untracked active gaps".to_string(),
        verify: "objective_progress = 1.0 on next eval".to_string(),
        machine_verify: VerifySpec::ScoreAbove { metric: "objective_progress", threshold: 1.0 },
        owner: "planner",
        evidence: "agent_state/OBJECTIVES.json",
    );

    plan!(
        metric: "safety",
        target: 1.0,
        score_key: "safety",
        score_default: 0.0,
        goal: "no active violations; semantic_fn_error_rate = 0".to_string(),
        trigger: format!(
            "safety={:.3}; check VIOLATIONS.json or non-zero semantic_fn_error_rate",
            eval_values.f64("safety")
        ),
        policy: "resolve_violations",
        action: "resolve all active violations in agent_state/VIOLATIONS.json first".to_string(),
        verify: "safety = 1.0 on next eval".to_string(),
        machine_verify: VerifySpec::ScoreAbove { metric: "safety", threshold: 1.0 },
        owner: "planner",
        evidence: "agent_state/VIOLATIONS.json",
    );

    plan!(
        metric: "task_velocity",
        target: 0.85,
        score_key: "task_velocity",
        score_default: 0.0,
        goal: "at least 85% of PLAN.json tasks complete".to_string(),
        trigger: format!(
            "task_velocity={:.3}; stale or incomplete tasks accumulating",
            eval_values.f64("task_velocity")
        ),
        policy: "close_stale_tasks",
        action: "mark completed tasks done; close tasks that will not execute this session"
            .to_string(),
        verify: "task_velocity >= 0.85 on next eval".to_string(),
        machine_verify: VerifySpec::ScoreAbove { metric: "task_velocity", threshold: 0.85 },
        owner: "planner",
        evidence: "agent_state/PLAN.json",
    );

    plan!(
        metric: "issue_health",
        target: 0.9,
        score_key: "issue_health",
        score_default: 0.0,
        goal: "repeated open issues resolved or closed".to_string(),
        trigger: format!(
            "issue_health={:.3}; repeated open issues without resolution",
            eval_values.f64("issue_health")
        ),
        policy: "fix_or_close_repeated_issues",
        action: "fix or close top repeated open issues in ISSUES.json by score descending"
            .to_string(),
        verify: "issue_health >= 0.9 on next eval".to_string(),
        machine_verify: VerifySpec::ScoreAbove { metric: "issue_health", threshold: 0.9 },
        owner: "executor",
        evidence: "agent_state/ISSUES.json",
    );

    {
        let error_rate = eval_values.f64("semantic_fn_error_rate");
        let low_conf = eval_values.f64("semantic_fn_low_confidence_rate");
        let low_conf_count = eval_values.u64("semantic_fn_low_confidence");
        let intent = eval_values.f64("semantic_fn_intent_coverage");
        plan!(
            metric: "semantic_contract",
            target: 0.50,
            score_key: "semantic_contract",
            score_default: 0.0,
            goal: "semantic_contract >= 0.50: intent coverage above 54%, \
                low-confidence rate below 46%".to_string(),
            trigger: format!(
                "fn_error_rate={error_rate:.4}  intent_coverage={intent:.4}  \
                low_confidence_rate={low_conf:.4}  ({low_conf_count} fns lack \
                structural intent signal)"
            ),
            policy: "enrich_semantic_intent_annotations",
            action: "for each unknown_low_confidence fn in \
                agent_state/semantic_manifest_proposals.json: \
                (1) read its effects/calls/resource fields from the manifest — \
                these are already extracted from graph.json; \
                (2) determine the correct intent class from structural evidence \
                (e.g. effects=[state_read] + returns bool → validation_gate, \
                effects=[none] + pure inputs/outputs → pure_transform, \
                calls=[append_blocker|record_action_failure] → event_append); \
                (3) add exactly two lines before the fn in source: \
                '/// Intent: {class}' and '/// Resource: {resource}'; \
                (4) run cargo check — the rustc wrapper re-analyzes and \
                reclassifies the fn from unknown_low_confidence to the \
                declared class. \
                Do NOT add generic labels — only annotate when the manifest \
                evidence (effects, calls, resource) clearly supports the class."
                .to_string(),
            verify: "semantic_intent increases on next eval (at least one fn \
                moves from unknown_low_confidence to a specific class)".to_string(),
            machine_verify: VerifySpec::ScoreAbove { metric: "semantic_contract", threshold: 0.50 },
            owner: "executor",
            evidence: "agent_state/semantic_manifest_proposals.json \
                (fn_low_confidence list with effects/calls/resource per fn), \
                state/rustc/canon_mini_agent/graph.json (intent_class, \
                effects edges per node)",
        );
    }

    {
        let missing = eval_values.arr_str("missing_structural_invariant_kinds");
        plan!(
            metric: "structural_invariant_coverage",
            target: 1.0,
            score_key: "structural_invariant_coverage",
            score_default: 0.0,
            goal: "all known graph structural risks have a matching enforced invariant"
                .to_string(),
            trigger: if missing.is_empty() {
                format!(
                    "structural_invariant_coverage={:.3}",
                    eval_values.f64("structural_invariant_coverage")
                )
            } else {
                format!("missing invariants for: {missing}")
            },
            policy: "synthesize_structural_invariant",
            action: "patch src/invariant_discovery.rs to synthesize the missing structural \
                invariant; do not edit enforced_invariants.json directly".to_string(),
            verify: "structural_invariant_coverage = 1.0; \
                missing_structural_invariant_kinds empty".to_string(),
            machine_verify: VerifySpec::ScoreAbove {
                metric: "structural_invariant_coverage",
                threshold: 1.0,
            },
            owner: "executor",
            evidence: "agent_state/enforced_invariants.json, \
                state/rustc/canon_mini_agent/graph.json",
        );
    }

    {
        let top = eval_values.string("blocker_top_uncovered");
        let distinct = eval_values.u64("blocker_distinct_classes");
        let covered = eval_values.u64("blocker_covered_classes");
        let score = eval_values.f64_or("blocker_class_coverage", 1.0);
        let mv = if top.is_empty() {
            VerifySpec::ScoreAbove { metric: "blocker_class_coverage", threshold: 1.0 }
        } else {
            VerifySpec::All(vec![
                VerifySpec::ScoreImproves { metric: "blocker_class_coverage", from: score },
                VerifySpec::FieldNotEquals {
                    key: "blocker_top_uncovered",
                    value: top.clone(),
                },
            ])
        };
        plan!(
            metric: "blocker_class_coverage",
            target: 1.0,
            score_key: "blocker_class_coverage",
            score_default: 1.0,
            goal: "every distinct runtime error class covered by an enforced invariant"
                .to_string(),
            trigger: format!(
                "{covered}/{distinct} classes covered; top uncovered: {}",
                if top.is_empty() { "none".to_string() } else { top.clone() }
            ),
            policy: "synthesize_blocker_invariant",
            action: if top.is_empty() {
                "no uncovered blocker classes; verify blockers.json is populated".to_string()
            } else {
                format!(
                    "patch src/invariant_discovery.rs — add detection rule for '{top}' \
                    emitting a typed invariant when support_count >= 3"
                )
            },
            verify: if top.is_empty() {
                "blocker_class_coverage = 1.0 on next eval".to_string()
            } else {
                format!(
                    "blocker_class_coverage > {score:.3} AND top_uncovered != {top} \
                    on next eval"
                )
            },
            machine_verify: mv,
            owner: "executor",
            evidence: "agent_state/blockers.json, agent_state/enforced_invariants.json",
        );
    }

    {
        let new_orphans = eval_values.u64("artifact_lineage_orphans_new");
        if new_orphans > 0 {
            push_eval_metric_plan(
                &mut plans,
                "artifact_lineage_orphans_new",
                0.0,
                1.0,
                "new artifact writes have complete lineage after migration boundary".to_string(),
                format!(
                    "artifact_lineage_orphans_new={new_orphans}; new artifact events lack \
                    artifact_id/source_event_seq/producer/target/eval linkage"
                ),
                "repair_artifact_lineage",
                "patch artifact write call sites so each new WorkspaceArtifactWriteRequested/Applied \
                event carries artifact_id, source_event_seq, producer_action, target, \
                repair_plan_id/plan_task_id when applicable, and eval_outcome".to_string(),
                "artifact_lineage_orphans_new = 0 on next eval".to_string(),
                VerifySpec::FieldEqualsU64 {
                    key: "artifact_lineage_orphans_new",
                    value: 0,
                },
                "executor",
                "agent_state/tlog.ndjson WorkspaceArtifactWriteRequested/Applied lineage fields",
            );
        }
    }

    {
        let bad_handoffs = eval_values.u64("handoff_without_ready");
        if bad_handoffs > 0 {
            push_eval_metric_plan(
                &mut plans,
                "handoff_without_ready",
                0.0,
                1.0,
                "planner handoff is blocked unless PLAN has at least one ready task".to_string(),
                format!(
                    "handoff_without_ready={bad_handoffs}; H=ready was observed while Q=0"
                ),
                "enforce_plan_ready_binding",
                "patch planner/executor route gates so status=ready requires \
                PLAN.ready_tasks>0; otherwise record planner_handoff_without_ready_tasks \
                and wake planner for auto-replan".to_string(),
                "handoff_without_ready = 0 on next eval".to_string(),
                VerifySpec::FieldEqualsU64 {
                    key: "handoff_without_ready",
                    value: 0,
                },
                "executor",
                "agent_state/tlog.ndjson BlockerRecorded(planner_handoff_without_ready_tasks)",
            );
        }
    }

    {
        let binding_rate = eval_values.f64_or("repair_plan_binding_rate", 1.0);
        plan!(
            metric: "repair_plan_binding_rate",
            target: 1.0,
            score_key: "repair_plan_binding_rate",
            score_default: 1.0,
            goal: "every active repair plan has a PLAN task carrying repair_plan_id and required_mutation".to_string(),
            trigger: format!("repair_plan_binding_rate={binding_rate:.3}; some repair work lacks deterministic PLAN binding"),
            policy: "enforce_repair_plan_binding",
            action: "update active PLAN repair tasks to include repair_plan_id, \
                required_mutation, target_files, and validation evidence copied from \
                the rendered REPAIR_PLAN block".to_string(),
            verify: "repair_plan_binding_rate = 1.0 on next eval".to_string(),
            machine_verify: VerifySpec::ScoreAbove {
                metric: "repair_plan_binding_rate",
                threshold: 1.0,
            },
            owner: "planner",
            evidence: "agent_state/PLAN.json, PlanVerifyRecorded(binding_checked=true)",
        );
    }

    {
        let truncations = eval_values.u64("tlog_prompt_truncation_count");
        let lag_ms = eval_values.u64("tlog_actionable_lag_total_ms");
        let payload = eval_values.string("tlog_dominant_payload_kind");
        plan!(
            metric: "canonical_delta_health",
            target: 0.9,
            score_key: "canonical_delta_health",
            score_default: 0.0,
            goal: "prompt truncations eliminated; actionable lag below 300 s".to_string(),
            trigger: format!(
                "prompt_truncations={truncations}  actionable_lag_ms={lag_ms}  \
                dominant_payload={payload}"
            ),
            policy: "reduce_prompt_pressure",
            action: format!(
                "reduce dominant payload '{payload}'; increase eval frequency to cut \
                actionable lag"
            ),
            verify: "canonical_delta_health >= 0.9; prompt_truncations decreasing".to_string(),
            machine_verify: VerifySpec::ScoreAbove {
                metric: "canonical_delta_health",
                threshold: 0.9,
            },
            owner: "planner",
            evidence: "agent_state/tlog.ndjson tlog_dominant_payload_kind, \
                tlog_prompt_truncation_count",
        );
    }

    {
        let unmeasured = eval_values.u64("unmeasured_improvement_attempts");
        let attempts = eval_values.u64("improvement_attempts");
        plan!(
            metric: "improvement_measurement",
            target: 1.0,
            score_key: "improvement_measurement",
            score_default: 1.0,
            goal: "every apply_patch improvement has a measured eval delta in tlog".to_string(),
            trigger: format!(
                "{unmeasured}/{attempts} improvement attempts have no follow-up eval"
            ),
            policy: "run_eval_after_patch",
            action: "after every apply_patch, run canon-generate-issues \
                --complexity-report-only before marking the task done".to_string(),
            verify: "improvement_measurement = 1.0; unmeasured_improvement_attempts = 0"
                .to_string(),
            machine_verify: VerifySpec::ScoreAbove {
                metric: "improvement_measurement",
                threshold: 1.0,
            },
            owner: "executor",
            evidence: "agent_state/tlog.ndjson unmeasured_improvement_attempts",
        );
    }

    {
        let unvalidated = eval_values.u64("unvalidated_improvement_attempts");
        let attempts = eval_values.u64("improvement_attempts");
        plan!(
            metric: "improvement_validation",
            target: 1.0,
            score_key: "improvement_validation",
            score_default: 1.0,
            goal: "every improvement attempt has cargo check/test verification in tlog"
                .to_string(),
            trigger: format!(
                "{unvalidated}/{attempts} improvements have no cargo result in tlog"
            ),
            policy: "verify_after_patch",
            action: "run cargo check -p canon-mini-agent immediately after every apply_patch"
                .to_string(),
            verify: "improvement_validation = 1.0; unvalidated_improvement_attempts = 0"
                .to_string(),
            machine_verify: VerifySpec::ScoreAbove {
                metric: "improvement_validation",
                threshold: 1.0,
            },
            owner: "executor",
            evidence: "agent_state/tlog.ndjson unvalidated_improvement_attempts",
        );
    }

    {
        let regressed = eval_values.u64("regressed_improvement_attempts");
        let measured = eval_values.u64("measured_improvement_attempts");
        plan!(
            metric: "improvement_effectiveness",
            target: 0.8,
            score_key: "improvement_effectiveness",
            score_default: 1.0,
            goal: "at least 80% of measured improvements raise the eval score".to_string(),
            trigger: format!(
                "{regressed}/{measured} measured improvements caused eval score regression"
            ),
            policy: "revert_regressing_patches",
            action: "identify regressing improvements in tlog (delta_g < 0 after apply_patch) \
                and revert or narrow their scope".to_string(),
            verify: "improvement_effectiveness >= 0.8; regressed_improvement_attempts stable"
                .to_string(),
            machine_verify: VerifySpec::ScoreAbove {
                metric: "improvement_effectiveness",
                threshold: 0.8,
            },
            owner: "executor",
            evidence: "agent_state/tlog.ndjson delta_g in eval_score_recorded",
        );
    }

    append_recovery_effectiveness_metric_plan(&eval_values, &mut plans);

    sort_repair_plans_by_priority_and_score(&mut plans);
    plans.truncate(max);
    plans
}

fn append_recovery_effectiveness_metric_plan(
    eval_values: &EvalMetricValues<'_>,
    plans: &mut Vec<RepairPlan>,
) {
    let failures = eval_values.u64("recovery_failures");
    let attempts = eval_values.u64("recovery_attempts");
    push_recovery_effectiveness_plan(
        plans,
        eval_values.f64_or("recovery_effectiveness", 1.0),
        failures,
        attempts,
    );
}

fn push_recovery_effectiveness_plan(
    plans: &mut Vec<RepairPlan>,
    score: f64,
    failures: u64,
    attempts: u64,
) {
    push_eval_metric_plan(
        plans,
        "recovery_effectiveness",
        score,
        1.0,
        "all typed recovery attempts resolve the blocker class".to_string(),
        format!("recovery_failures={failures}/{attempts}; some policies not resolving blocker"),
        "inspect_failed_recovery",
        "inspect recovery_outcome_recorded events in tlog where success=false; patch the failing policy in src/recovery.rs".to_string(),
        "recovery_effectiveness = 1.0; recovery_failures = 0".to_string(),
        VerifySpec::ScoreAbove { metric: "recovery_effectiveness", threshold: 1.0 },
        "executor",
        "agent_state/tlog.ndjson recovery_outcome_recorded with success=false",
    );
}

fn eval_array_string(eval: &Map<String, Value>, key: &str) -> String {
    eval.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

fn sort_repair_plans_by_priority_and_score(plans: &mut [RepairPlan]) {
    plans.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
    });
}

// ── snapshot → eval map ───────────────────────────────────────────────────────

/// Convert a live `EvaluationWorkspaceSnapshot` to the eval JSON map that
/// `build_eval_metric_plans` and `VerifySpec::check` consume.
/// Used by `eval_driver` so it doesn't need to re-read `latest.json`.
pub fn snapshot_to_eval_map(
    snapshot: &crate::evaluation::EvaluationWorkspaceSnapshot,
) -> Map<String, Value> {
    let v = &snapshot.vector;
    let t = &snapshot.tlog_delta_signals;
    let b = &snapshot.blocker_class_coverage;
    let s = &snapshot.structural_invariant_coverage;
    let mut m = Map::new();
    append_snapshot_vector_eval_fields(v, &mut m);
    m.insert("blocker_distinct_classes".into(), json!(b.distinct_classes));
    m.insert("blocker_covered_classes".into(), json!(b.covered_classes));
    m.insert(
        "blocker_top_uncovered".into(),
        json!(b.top_uncovered.as_deref().unwrap_or("")),
    );
    m.insert("missing_structural_invariant_kinds".into(), json!(s.missing));
    m.insert("tlog_prompt_truncation_count".into(), json!(t.prompt_truncations));
    m.insert("tlog_actionable_lag_total_ms".into(), json!(t.actionable_lag_total_ms));
    m.insert("tlog_dominant_payload_kind".into(), json!(t.dominant_payload_kind));
    m.insert("improvement_attempts".into(), json!(t.improvement_attempts));
    m.insert("unmeasured_improvement_attempts".into(), json!(t.unmeasured_improvement_attempts));
    m.insert("unvalidated_improvement_attempts".into(), json!(t.unvalidated_improvement_attempts));
    m.insert("regressed_improvement_attempts".into(), json!(t.regressed_improvement_attempts));
    m.insert("measured_improvement_attempts".into(), json!(t.measured_improvement_attempts));
    m.insert("recovery_attempts".into(), json!(t.recovery_attempts));
    m.insert("recovery_failures".into(), json!(t.recovery_failures));
    m.insert(
        "artifact_lineage_orphans_new".into(),
        json!(t.artifact_lineage_orphans_new),
    );
    m.insert(
        "artifact_lineage_complete".into(),
        json!(t.artifact_lineage_complete),
    );
    m.insert(
        "artifact_lineage_orphans".into(),
        json!(t.artifact_lineage_orphans),
    );
    m.insert("orphan_artifact_ids".into(), json!(&t.orphan_artifact_ids));
    m.insert("handoff_without_ready".into(), json!(t.handoff_without_ready));
    m.insert(
        "repair_plan_binding_rate".into(),
        json!(t.repair_plan_binding_rate),
    );
    m.insert(
        "artifact_lineage_migration_seq_boundary".into(),
        json!(t.artifact_lineage_migration_seq_boundary),
    );
    append_snapshot_semantic_eval_fields(snapshot, &mut m);
    m
}

fn append_snapshot_vector_eval_fields(
    v: &crate::evaluation::EvaluationVector,
    m: &mut Map<String, Value>,
) {
    m.insert("objective_progress".into(), json!(v.objective_progress));
    m.insert("safety".into(), json!(v.safety));
    m.insert("task_velocity".into(), json!(v.task_velocity));
    m.insert("issue_health".into(), json!(v.issue_health));
    m.insert("semantic_contract".into(), json!(v.semantic_contract));
    m.insert("structural_invariant_coverage".into(), json!(v.structural_invariant_coverage));
    m.insert("blocker_class_coverage".into(), json!(v.blocker_class_coverage));
    m.insert("canonical_delta_health".into(), json!(v.canonical_delta_health));
    m.insert("improvement_measurement".into(), json!(v.improvement_measurement));
    m.insert("improvement_validation".into(), json!(v.improvement_validation));
    m.insert("improvement_effectiveness".into(), json!(v.improvement_effectiveness));
    m.insert("recovery_effectiveness".into(), json!(v.recovery_effectiveness));
}

fn append_snapshot_semantic_eval_fields(
    snapshot: &crate::evaluation::EvaluationWorkspaceSnapshot,
    m: &mut Map<String, Value>,
) {
    m.insert("semantic_fn_error_rate".into(), json!(snapshot.semantic_fn_error_rate));
    m.insert("semantic_fn_intent_coverage".into(), json!(snapshot.semantic_fn_intent_coverage));
    m.insert(
        "semantic_fn_low_confidence_rate".into(),
        json!(snapshot.semantic_fn_low_confidence_rate),
    );
}

// ── Tlog scanning helpers ─────────────────────────────────────────────────────

/// Count how many consecutive `PlanVerifyRecorded(passed=false)` events exist
/// for `plan_id` at the tail of the tlog (most-recent-first scan, stops at
/// first `passed=true`).  Returns 0 if no failures or if tlog is unreadable.
pub fn count_consecutive_verify_failures(workspace: &Path, plan_id: &str) -> usize {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let records = match crate::tlog::Tlog::read_recent_records(&tlog_path, 300) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut count = 0usize;
    for record in records.iter().rev() {
        if let crate::events::Event::Effect {
            event:
                crate::events::EffectEvent::PlanVerifyRecorded {
                    plan_id: pid,
                    passed,
                    ..
                },
        } = &record.event
        {
            if pid == plan_id {
                if *passed {
                    break;
                }
                count += 1;
            }
        }
    }
    count
}

/// Return (plan_id, passed, consecutive_failures) for the most recent verify
/// result of every plan seen in recent tlog — used by prompt to surface status.
pub fn recent_plan_verify_outcomes(
    workspace: &Path,
) -> Vec<(String, bool, usize)> {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let records = match crate::tlog::Tlog::read_recent_records(&tlog_path, 500) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    // latest result per plan (walk forward so last write wins)
    let mut latest: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    for record in &records {
        if let crate::events::Event::Effect {
            event:
                crate::events::EffectEvent::PlanVerifyRecorded {
                    plan_id, passed, ..
                },
        } = &record.event
        {
            latest.insert(plan_id.clone(), *passed);
        }
    }

    let mut out: Vec<(String, bool, usize)> = latest
        .into_iter()
        .map(|(id, passed)| {
            let failures = if passed {
                0
            } else {
                count_consecutive_verify_failures(workspace, &id)
            };
            (id, passed, failures)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn all_weak_eval() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("objective_progress".into(), json!(0.5));
        m.insert("safety".into(), json!(0.85));
        m.insert("task_velocity".into(), json!(0.5));
        m.insert("issue_health".into(), json!(0.5));
        m.insert("semantic_contract".into(), json!(0.28));
        m.insert("semantic_fn_error_rate".into(), json!(0.0));
        m.insert("semantic_fn_low_confidence_rate".into(), json!(0.46));
        m.insert("semantic_fn_intent_coverage".into(), json!(0.54));
        m.insert("structural_invariant_coverage".into(), json!(0.8));
        m.insert("missing_structural_invariant_kinds".into(), json!(["issues_projection_only"]));
        m.insert("blocker_class_coverage".into(), json!(0.33));
        m.insert("blocker_distinct_classes".into(), json!(3u64));
        m.insert("blocker_covered_classes".into(), json!(1u64));
        m.insert("blocker_top_uncovered".into(), json!("llm_timeout"));
        m.insert("canonical_delta_health".into(), json!(0.8));
        m.insert("tlog_prompt_truncation_count".into(), json!(39u64));
        m.insert("tlog_actionable_lag_total_ms".into(), json!(1_200_000u64));
        m.insert("tlog_dominant_payload_kind".into(), json!("enforced_invariants_recorded"));
        m.insert("improvement_measurement".into(), json!(0.83));
        m.insert("unmeasured_improvement_attempts".into(), json!(1u64));
        m.insert("improvement_attempts".into(), json!(6u64));
        m.insert("improvement_validation".into(), json!(0.9));
        m.insert("unvalidated_improvement_attempts".into(), json!(1u64));
        m.insert("improvement_effectiveness".into(), json!(0.67));
        m.insert("regressed_improvement_attempts".into(), json!(2u64));
        m.insert("measured_improvement_attempts".into(), json!(6u64));
        m.insert("recovery_effectiveness".into(), json!(0.8));
        m.insert("recovery_failures".into(), json!(1u64));
        m.insert("recovery_attempts".into(), json!(5u64));
        m
    }

    #[test]
    fn every_plan_has_required_fields() {
        let plans = build_eval_metric_plans(&all_weak_eval(), 12);
        assert!(!plans.is_empty());
        for p in &plans {
            assert!(!p.action.is_empty(), "{} empty action", p.id);
            assert!(!p.verify.is_empty(), "{} empty verify", p.id);
            assert!(!p.goal.is_empty(), "{} empty goal", p.id);
            assert!(!p.trigger.is_empty(), "{} empty trigger", p.id);
            assert!(
                !p.required_mutation.is_empty(),
                "{} empty required_mutation",
                p.id
            );
        }
    }

    #[test]
    fn pass_scores_emit_no_plans() {
        let mut m = Map::new();
        for key in &[
            "objective_progress", "safety", "task_velocity", "issue_health",
            "semantic_contract", "structural_invariant_coverage", "canonical_delta_health",
            "improvement_measurement", "improvement_validation",
            "improvement_effectiveness", "recovery_effectiveness",
        ] {
            m.insert((*key).to_string(), json!(1.0));
        }
        m.insert("blocker_class_coverage".into(), json!(1.0));
        m.insert("blocker_distinct_classes".into(), json!(0u64));
        m.insert("blocker_covered_classes".into(), json!(0u64));
        m.insert("blocker_top_uncovered".into(), json!(""));
        let plans = build_eval_metric_plans(&m, 12);
        assert!(plans.is_empty(), "got plans: {:?}",
            plans.iter().map(|p| &p.id).collect::<Vec<_>>());
    }

    #[test]
    fn plans_sorted_by_priority_then_score() {
        let plans = build_eval_metric_plans(&all_weak_eval(), 12);
        for w in plans.windows(2) {
            assert!(w[0].priority <= w[1].priority,
                "priority: {} ({}) before {} ({})",
                w[0].id, w[0].priority, w[1].id, w[1].priority);
            if w[0].priority == w[1].priority {
                assert!(w[0].score <= w[1].score + 0.001,
                    "score within prio {}: {} ({:.3}) before {} ({:.3})",
                    w[0].priority, w[0].id, w[0].score, w[1].id, w[1].score);
            }
        }
    }

    #[test]
    fn max_count_capped() {
        assert!(build_eval_metric_plans(&all_weak_eval(), 3).len() <= 3);
    }

    #[test]
    fn stable_id_format() {
        for p in build_eval_metric_plans(&all_weak_eval(), 12) {
            assert!(p.id.starts_with("eval_metric:"), "id '{}' missing prefix", p.id);
        }
    }

    #[test]
    fn blocker_plan_names_uncovered_class() {
        let plans = build_eval_metric_plans(&all_weak_eval(), 12);
        let bp = plans.iter()
            .find(|p| p.id == "eval_metric:blocker_class_coverage")
            .expect("blocker_class_coverage should be weak");
        assert!(bp.action.contains("llm_timeout"), "action: {}", bp.action);
        assert!(bp.verify.contains("llm_timeout"), "verify: {}", bp.verify);
    }

    #[test]
    fn blocker_plan_machine_verify_is_all_when_top_uncovered_set() {
        let plans = build_eval_metric_plans(&all_weak_eval(), 12);
        let bp = plans.iter()
            .find(|p| p.id == "eval_metric:blocker_class_coverage")
            .expect("blocker_class_coverage should be weak");
        assert!(matches!(bp.machine_verify, VerifySpec::All(_)),
            "expected All spec when top_uncovered is set");
    }

    // ── VerifySpec::check ─────────────────────────────────────────────────────

    #[test]
    fn score_above_passes_at_threshold() {
        let mut m = Map::new();
        m.insert("x".into(), json!(0.9));
        assert!(VerifySpec::ScoreAbove { metric: "x", threshold: 0.9 }.check(&m, ""));
    }

    #[test]
    fn score_above_fails_below_threshold() {
        let mut m = Map::new();
        m.insert("x".into(), json!(0.89));
        assert!(!VerifySpec::ScoreAbove { metric: "x", threshold: 0.9 }.check(&m, ""));
    }

    #[test]
    fn score_improves_passes_when_higher() {
        let mut m = Map::new();
        m.insert("x".into(), json!(0.5));
        assert!(VerifySpec::ScoreImproves { metric: "x", from: 0.33 }.check(&m, ""));
    }

    #[test]
    fn score_improves_fails_when_same() {
        let mut m = Map::new();
        m.insert("x".into(), json!(0.33));
        assert!(!VerifySpec::ScoreImproves { metric: "x", from: 0.33 }.check(&m, ""));
    }

    #[test]
    fn field_not_equals_passes_when_different() {
        let mut m = Map::new();
        m.insert("k".into(), json!("other"));
        assert!(VerifySpec::FieldNotEquals { key: "k", value: "llm_timeout".into() }.check(&m, ""));
    }

    #[test]
    fn field_not_equals_fails_when_same() {
        let mut m = Map::new();
        m.insert("k".into(), json!("llm_timeout"));
        assert!(!VerifySpec::FieldNotEquals { key: "k", value: "llm_timeout".into() }.check(&m, ""));
    }

    #[test]
    fn field_equals_u64_checks_numeric_count() {
        let mut m = Map::new();
        m.insert("handoff_without_ready".into(), json!(0u64));

        assert!(VerifySpec::FieldEqualsU64 {
            key: "handoff_without_ready",
            value: 0,
        }
        .check(&m, ""));
    }

    #[test]
    fn hard_gate_metrics_emit_repair_plans_when_nonzero() {
        let mut m = Map::new();
        m.insert("artifact_lineage_orphans_new".into(), json!(2u64));
        m.insert("handoff_without_ready".into(), json!(1u64));
        m.insert("repair_plan_binding_rate".into(), json!(0.5));

        let ids: Vec<String> = build_eval_metric_plans(&m, 12)
            .into_iter()
            .map(|plan| plan.id)
            .collect();

        assert!(ids.contains(&"eval_metric:artifact_lineage_orphans_new".to_string()));
        assert!(ids.contains(&"eval_metric:handoff_without_ready".to_string()));
        assert!(ids.contains(&"eval_metric:repair_plan_binding_rate".to_string()));
    }

    #[test]
    fn all_passes_when_all_pass() {
        let mut m = Map::new();
        m.insert("x".into(), json!(1.0));
        m.insert("k".into(), json!("other"));
        let spec = VerifySpec::All(vec![
            VerifySpec::ScoreAbove { metric: "x", threshold: 0.9 },
            VerifySpec::FieldNotEquals { key: "k", value: "llm_timeout".into() },
        ]);
        assert!(spec.check(&m, ""));
    }

    #[test]
    fn all_fails_when_any_fails() {
        let mut m = Map::new();
        m.insert("x".into(), json!(1.0));
        m.insert("k".into(), json!("llm_timeout"));
        let spec = VerifySpec::All(vec![
            VerifySpec::ScoreAbove { metric: "x", threshold: 0.9 },
            VerifySpec::FieldNotEquals { key: "k", value: "llm_timeout".into() },
        ]);
        assert!(!spec.check(&m, ""));
    }

    #[test]
    fn invariant_resolved_passes_for_enforced() {
        let json = r#"{"invariants":[{"id":"INV-abc","status":"enforced","predicate_text":"","support_count":1}]}"#;
        assert!(VerifySpec::InvariantResolved { id: "INV-abc".into() }.check(&Map::new(), json));
    }

    #[test]
    fn invariant_resolved_fails_for_promoted() {
        let json = r#"{"invariants":[{"id":"INV-abc","status":"promoted","predicate_text":"","support_count":1}]}"#;
        assert!(!VerifySpec::InvariantResolved { id: "INV-abc".into() }.check(&Map::new(), json));
    }

    // ── invariant and blocker class plans ─────────────────────────────────────

    #[test]
    fn invariant_plan_for_promoted_only() {
        let json = r#"{"version":1,"invariants":[
            {"id":"INV-p","predicate_text":"repeated failure","status":"promoted","support_count":12,"gates":["executor"]},
            {"id":"INV-e","predicate_text":"already ok","status":"enforced","support_count":5,"gates":["route"]}
        ]}"#;
        let plans = build_invariant_plans(json, 10);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "invariant:INV-p");
        assert!(matches!(&plans[0].machine_verify,
            VerifySpec::InvariantResolved { id } if id == "INV-p"));
    }

    #[test]
    fn blocker_class_plan_for_uncovered() {
        let blockers = r#"{"version":1,"blockers":[
            {"id":"b1","error_class":"llm_timeout","actor":"planner","summary":"t","action_kind":"llm_request","source":"action_result","ts_ms":1},
            {"id":"b2","error_class":"llm_timeout","actor":"planner","summary":"t","action_kind":"llm_request","source":"action_result","ts_ms":2}
        ]}"#;
        let invariants = r#"{"invariants":[{"predicate_text":"only missing_target","status":"enforced","support_count":1,"gates":[]}]}"#;
        let plans = build_blocker_class_plans(blockers, invariants, 10);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "blocker_class:llm_timeout");
        assert!(plans[0]
            .plan_mutation_template
            .contains("repair_plan_id=\"blocker_class:llm_timeout\""));
        assert_eq!(plans[0].required_mutation, "synthesize_blocker_invariant");
        assert_eq!(plans[0].target_files, vec!["src/invariant_discovery.rs"]);
        assert!(plans[0]
            .persisted_policy
            .contains("recovery_policy=retire_transport_and_retry"));
        assert!(matches!(&plans[0].machine_verify,
            VerifySpec::FieldNotEquals { key: "blocker_top_uncovered", value }
                if value == "llm_timeout"));
    }

    #[test]
    fn plan_binding_verifier_requires_exact_mutation_and_targets() {
        let mut plan = build_blocker_class_plans(
            r#"{"blockers":[{"error_class":"llm_timeout"}]}"#,
            r#"{"invariants":[]}"#,
            10,
        )
        .remove(0);
        plan.machine_verify = VerifySpec::FieldNotEquals {
            key: "blocker_top_uncovered",
            value: "llm_timeout".to_string(),
        };

        let good_plan = json!({
            "tasks": [{
                "id": "T1",
                "status": "ready",
                "repair_plan_id": "blocker_class:llm_timeout",
                "required_mutation": "synthesize_blocker_invariant",
                "target_files": ["src/invariant_discovery.rs"]
            }]
        })
        .to_string();
        assert!(verify_plan_binding(&plan, &good_plan).passed);

        let drifted_plan = json!({
            "tasks": [{
                "id": "T1",
                "status": "ready",
                "repair_plan_id": "blocker_class:llm_timeout",
                "required_mutation": "generic_cleanup",
                "target_files": []
            }]
        })
        .to_string();
        let result = verify_plan_binding(&plan, &drifted_plan);
        assert!(!result.passed);
        assert!(result.description.contains("required_mutation"));
        assert!(result
            .description
            .contains("target_files contains src/invariant_discovery.rs"));
    }

    // ── render ────────────────────────────────────────────────────────────────

    #[test]
    fn rendered_plan_has_all_fields() {
        let r = render_plan(&build_eval_metric_plans(&all_weak_eval(), 1)[0]);
        for f in &[
            "REPAIR_PLAN", "kind:", "id:", "goal:", "trigger:", "policy:",
            "required_mutation:", "target_files:", "action:", "plan_mutation_template:", "persisted_policy:",
            "verify:", "machine_verify:", "owner:", "evidence:",
        ] {
            assert!(r.contains(f), "missing: {f}\n{r}");
        }
    }
}
