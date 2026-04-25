fn handle_plan_sorted_view_action(workspace: &Path) -> Result<(bool, String)> {
    let (obj, tasks, edges) = load_plan_components(workspace)?;
    let task_map = build_task_map(&tasks);
    let order = topo_sort_plan(&task_map, &edges)?;
    let ordered_tasks = collect_ordered_tasks(&task_map, &order);
    let rendered = render_plan_sorted_view_output(&obj, order, ordered_tasks, &edges)?;
    Ok((false, rendered))
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<(serde_json::Map<std::string::String, serde_json::Value>, std::vec::Vec<serde_json::Value>, std::vec::Vec<serde_json::Value>), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_plan_components(
    workspace: &Path,
) -> Result<(serde_json::Map<String, Value>, Vec<Value>, Vec<Value>)> {
    let plan_path = workspace.join(MASTER_PLAN_FILE);
    let plan = load_or_init_plan(&plan_path)?;
    let obj = plan
        .as_object()
        .ok_or_else(|| anyhow!("PLAN.json must be a JSON object"))?;
    let tasks = obj
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    let edges = obj
        .get("dag")
        .and_then(|v| v.as_object())
        .and_then(|d| d.get("edges"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
    ensure_dag(tasks, edges)?;
    Ok((obj.clone(), tasks.clone(), edges.clone()))
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &[serde_json::Value]
/// Outputs: std::collections::HashMap<std::string::String, serde_json::Value>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_task_map(tasks: &[Value]) -> std::collections::HashMap<String, Value> {
    let mut task_map = std::collections::HashMap::new();
    for task in tasks {
        if let Some(id) = task.get("id").and_then(|v| v.as_str()) {
            task_map.insert(id.to_string(), task.clone());
        }
    }
    task_map
}

fn topo_sort_plan(
    task_map: &std::collections::HashMap<String, Value>,
    edges: &[Value],
) -> Result<Vec<String>> {
    let (mut indegree, adj) = build_plan_sorted_graph_state(task_map, edges)?;
    let mut ready: BTreeSet<String> = indegree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut order = Vec::new();
    while let Some(id) = take_next_ready_plan_node(&mut ready) {
        order.push(id.clone());
        drain_plan_sorted_successors(&adj, &id, &mut indegree, &mut ready);
    }
    Ok(order)
}

fn collect_ordered_tasks(
    task_map: &std::collections::HashMap<String, Value>,
    order: &[String],
) -> Vec<Value> {
    let mut ordered = Vec::new();
    for id in order {
        if let Some(task) = task_map.get(id) {
            ordered.push(task.clone());
        }
    }
    ordered
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::collections::HashMap<std::string::String, serde_json::Value>, &[serde_json::Value]
/// Outputs: std::result::Result<(std::collections::HashMap<std::string::String, usize>, std::collections::HashMap<std::string::String, std::collections::BTreeSet<std::string::String>>), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_plan_sorted_graph_state(
    task_map: &std::collections::HashMap<String, Value>,
    edges: &[Value],
) -> Result<(
    std::collections::HashMap<String, usize>,
    std::collections::HashMap<String, BTreeSet<String>>,
)> {
    let mut indegree: std::collections::HashMap<String, usize> =
        task_map.keys().map(|k| (k.clone(), 0)).collect();
    let mut adj: std::collections::HashMap<String, BTreeSet<String>> =
        std::collections::HashMap::new();
    for id in task_map.keys() {
        adj.insert(id.clone(), BTreeSet::new());
    }
    for edge in edges {
        let from = edge.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let to = edge.get("to").and_then(|v| v.as_str()).unwrap_or("");
        if from.is_empty() || to.is_empty() {
            bail!(format!(
                "plan edge missing from/to; candidate edge: {}",
                serde_json::to_string(edge).unwrap_or_else(|_| "<invalid edge json>".to_string())
            ));
        }
        insert_plan_edge_adjacency(&mut adj, from, to);
        increment_plan_edge_indegree(&mut indegree, to);
    }
    Ok((indegree, adj))
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Map<std::string::String, serde_json::Value>, std::vec::Vec<std::string::String>, std::vec::Vec<serde_json::Value>, &[serde_json::Value]
/// Outputs: std::result::Result<std::string::String, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn render_plan_sorted_view_output(
    obj: &serde_json::Map<String, Value>,
    order: Vec<String>,
    ordered_tasks: Vec<Value>,
    edges: &[Value],
) -> Result<String> {
    let mut output = serde_json::Map::new();
    copy_plan_view_metadata(obj, &mut output);
    insert_plan_view_collections(&mut output, order, ordered_tasks, edges);
    Ok(serde_json::to_string_pretty(&Value::Object(output))?)
}

fn copy_plan_view_metadata(
    obj: &serde_json::Map<String, Value>,
    output: &mut serde_json::Map<String, Value>,
) {
    for key in ["version", "status"] {
        if let Some(value) = obj.get(key) {
            output.insert(key.to_string(), value.clone());
        }
    }
}

fn insert_plan_view_collections(
    output: &mut serde_json::Map<String, Value>,
    order: Vec<String>,
    ordered_tasks: Vec<Value>,
    edges: &[Value],
) {
    output.insert(
        "order".to_string(),
        Value::Array(order.into_iter().map(Value::String).collect()),
    );
    output.insert("tasks".to_string(), Value::Array(ordered_tasks));
    output.insert("edges".to_string(), Value::Array(edges.to_vec()));
}

fn insert_plan_edge_adjacency(
    adj: &mut std::collections::HashMap<String, BTreeSet<String>>,
    from: &str,
    to: &str,
) {
    if let Some(nexts) = adj.get_mut(from) {
        nexts.insert(to.to_string());
    }
}

fn increment_plan_edge_indegree(indegree: &mut std::collections::HashMap<String, usize>, to: &str) {
    if let Some(count) = indegree.get_mut(to) {
        *count += 1;
    }
}

fn take_next_ready_plan_node(ready: &mut BTreeSet<String>) -> Option<String> {
    let id = ready.iter().next().cloned()?;
    ready.remove(&id);
    Some(id)
}

fn drain_plan_sorted_successors(
    adj: &std::collections::HashMap<String, BTreeSet<String>>,
    id: &str,
    indegree: &mut std::collections::HashMap<String, usize>,
    ready: &mut BTreeSet<String>,
) {
    if let Some(nexts) = adj.get(id) {
        for next in nexts {
            update_plan_sorted_successor_indegree(indegree, ready, next);
        }
    }
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &mut std::collections::HashMap<std::string::String, usize>, &mut std::collections::BTreeSet<std::string::String>, &str
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn update_plan_sorted_successor_indegree(
    indegree: &mut std::collections::HashMap<String, usize>,
    ready: &mut BTreeSet<String>,
    next: &str,
) {
    if let Some(count) = indegree.get_mut(next) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            ready.insert(next.to_string());
        }
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::option::Option<std::path::PathBuf>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_output_log_path(out: &str) -> Option<PathBuf> {
    for line in out.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("output_log:") {
            let path = rest.trim();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        if let Some(idx) = trimmed.find("output_log=") {
            let rest = trimmed[idx + "output_log=".len()..].trim();
            let path = rest.split_whitespace().next().unwrap_or("");
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_cargo_test_failures(out: &str) -> Value {
    let mut locations = BTreeSet::new();
    let mut failed_tests = BTreeSet::new();
    let mut stalled_tests = BTreeSet::new();
    let mut failure_block: Vec<String> = Vec::new();
    let mut rerun_hint: Option<String> = None;

    let (log_path, scan) = load_cargo_test_failure_scan(out);

    for line in scan.lines() {
        let trimmed = line.trim();
        parse_cargo_test_failure_line(
            trimmed,
            &mut locations,
            &mut failed_tests,
            &mut stalled_tests,
            &mut failure_block,
            &mut rerun_hint,
        );
    }

    let mut payload = serde_json::Map::new();
    payload.insert(
        "error_locations".to_string(),
        Value::Array(locations.into_iter().map(Value::String).collect()),
    );
    if let Some(path) = log_path {
        payload.insert(
            "output_log".to_string(),
            Value::String(path.display().to_string()),
        );
    }
    if !stalled_tests.is_empty() {
        payload.insert(
            "stalled_tests".to_string(),
            Value::Array(stalled_tests.iter().cloned().map(Value::String).collect()),
        );
        if failed_tests.is_empty() {
            failed_tests.extend(stalled_tests.iter().cloned());
        }
        if rerun_hint.is_none() {
            rerun_hint =
                Some("tests appear stalled; re-run with timeout or inspect output_log".to_string());
        }
    }
    if !failed_tests.is_empty() {
        payload.insert(
            "failed_tests".to_string(),
            Value::Array(failed_tests.into_iter().map(Value::String).collect()),
        );
    }
    if let Some(hint) = rerun_hint {
        payload.insert("rerun_hint".to_string(), Value::String(hint));
    }
    if !failure_block.is_empty() {
        payload.insert(
            "failure_block".to_string(),
            Value::Array(failure_block.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(payload)
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &mut std::collections::BTreeSet<std::string::String>, &mut std::collections::BTreeSet<std::string::String>, &mut std::collections::BTreeSet<std::string::String>, &mut std::vec::Vec<std::string::String>, &mut std::option::Option<std::string::String>
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_cargo_test_failure_line(
    trimmed: &str,
    locations: &mut BTreeSet<String>,
    failed_tests: &mut BTreeSet<String>,
    stalled_tests: &mut BTreeSet<String>,
    failure_block: &mut Vec<String>,
    rerun_hint: &mut Option<String>,
) {
    if let Some(location) = cargo_test_error_location(trimmed) {
        locations.insert(location);
    }
    if let Some(stripped) = cargo_test_test_line(trimmed) {
        insert_failed_test_name(failed_tests, stripped);
        collect_stalled_test_name(stalled_tests, stripped);
    }
    if let Some(hint) = cargo_test_rerun_hint(trimmed) {
        rerun_hint.get_or_insert(hint);
    }
    if is_cargo_test_failure_block_line(trimmed) {
        failure_block.push(trimmed.to_string());
    }
}

fn cargo_test_error_location(trimmed: &str) -> Option<String> {
    let idx = trimmed.find(".rs:")?;
    let path = &trimmed[..idx + 3];
    let rest = &trimmed[idx + 3..];
    let mut it = rest.splitn(3, ':');
    let line_no = it.next().unwrap_or("");
    let col_no = it.next().unwrap_or("");
    if line_no.is_empty() || col_no.is_empty() {
        return None;
    }
    Some(format!("{}:{}:{}", path, line_no, col_no))
}

fn cargo_test_test_line(trimmed: &str) -> Option<&str> {
    trimmed.strip_prefix("test ")
}

fn insert_failed_test_name(failed_tests: &mut BTreeSet<String>, stripped: &str) {
    if let Some(name) = stripped.strip_suffix(" ... FAILED") {
        failed_tests.insert(name.trim().to_string());
    }
}

fn cargo_test_rerun_hint(trimmed: &str) -> Option<String> {
    trimmed.contains("To rerun").then(|| trimmed.to_string())
}

fn is_cargo_test_failure_block_line(trimmed: &str) -> bool {
    trimmed.contains("panicked at")
        || trimmed.contains("FAILED")
        || trimmed.contains("has been running for over")
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &str
/// Outputs: (std::option::Option<std::path::PathBuf>, std::string::String)
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_cargo_test_failure_scan(out: &str) -> (Option<PathBuf>, String) {
    let log_path = extract_output_log_path(out);
    let mut scan = out.to_string();
    if let Some(path) = log_path.as_ref() {
        if let Ok(content) = fs::read_to_string(path) {
            scan = truncate(&content, MAX_SNIPPET * 4).to_string();
        }
    }
    (log_path, scan)
}

fn collect_stalled_test_name(stalled_tests: &mut BTreeSet<String>, stripped: &str) {
    let stalled_test = stripped
        .rsplit_once(" has been running for over ")
        .and_then(|(name, tail)| {
            tail.strip_suffix(" seconds")
                .filter(|seconds_raw| seconds_raw.trim().parse::<u64>().is_ok())
                .map(|_| name.trim().to_string())
        });
    if let Some(name) = stalled_test {
        stalled_tests.insert(name);
    }
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<std::collections::HashMap<u32, (std::string::String, std::string::String)>, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_graph_symbols(
    graph_json: &Path,
) -> Result<std::collections::HashMap<u32, (String, String)>> {
    let content = ctx_read(graph_json)?;
    let value: Value = serde_json::from_str(&content)?;
    let mut out = std::collections::HashMap::new();
    let nodes = value
        .get("nodes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for node in nodes {
        let id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if id == 0 {
            continue;
        }
        let kind = node
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let symbol = node
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.insert(id, (kind, symbol));
    }
    Ok(out)
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<std::collections::HashMap<u32, (std::string::String, std::string::String)>, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_nodes_symbols(
    nodes_csv: &Path,
) -> Result<std::collections::HashMap<u32, (String, String)>> {
    let content = ctx_read(nodes_csv)?;
    let mut out = std::collections::HashMap::new();
    for (idx, line) in content.lines().enumerate() {
        if idx == 0 {
            continue;
        }
        let mut parts = line.splitn(4, ',');
        let id_raw = parts.next().unwrap_or("").trim();
        let kind = parts.next().unwrap_or("").trim().to_string();
        let symbol = parts.next().unwrap_or("").trim().to_string();
        if let Ok(id) = id_raw.parse::<u32>() {
            if id == 0 {
                continue;
            }
            out.insert(id, (kind, symbol));
        }
    }
    Ok(out)
}

fn symbol_label(map: &std::collections::HashMap<u32, (String, String)>, raw: &str) -> String {
    let id = raw.parse::<u32>().ok();
    if let Some(id) = id {
        if let Some((kind, symbol)) = map.get(&id) {
            if symbol.is_empty() {
                return format!("{id} {kind}").trim().to_string();
            }
            return format!("{id} {kind} {symbol}").trim().to_string();
        }
        return id.to_string();
    }
    raw.to_string()
}

pub(crate) fn patch_scope_error_with_mode(
    role: &str,
    patch: &str,
    self_mod: bool,
) -> Option<String> {
    let raw_targets = patch_targets(patch);
    let normalized_targets: Vec<String> = raw_targets
        .iter()
        .map(|target| normalize_patch_target_for_scope(target))
        .collect();
    let targets: Vec<&str> = normalized_targets.iter().map(|s| s.as_str()).collect();
    if targets.is_empty() {
        return None;
    }

    let touches = classify_patch_targets(&targets);
    match role {
        "solo" => None,
        role if role.starts_with("executor") => executor_patch_scope_error(&touches, self_mod),
        "verifier" | "verifier_a" | "verifier_b" => verifier_patch_scope_error(&touches),
        "planner" | "mini_planner" => planner_patch_scope_error(&targets, &touches),
        "diagnostics" => diagnostics_patch_scope_error(&touches),
        _ => None,
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn normalize_patch_target_for_scope(target: &str) -> String {
    let trimmed = target.trim();
    let target_path = Path::new(trimmed);
    if target_path.is_absolute() {
        let ws = Path::new(crate::constants::workspace());
        if let Ok(relative) = target_path.strip_prefix(ws) {
            return relative.to_string_lossy().replace('\\', "/");
        }
    }
    trimmed.strip_prefix("./").unwrap_or(trimmed).to_string()
}

struct PatchTargetTouches {
    diagnostics_file: String,
    legacy_diagnostics_file: &'static str,
    touches_spec: bool,
    touches_lane: bool,
    touches_master_plan: bool,
    touches_violations: bool,
    touches_objectives: bool,
    touches_diagnostics: bool,
    touches_src: bool,
    touches_tests: bool,
    touches_other: bool,
}

fn classify_patch_targets(targets: &[&str]) -> PatchTargetTouches {
    let diagnostics_file = diagnostics_file().to_string();
    let legacy_diagnostics_file = "DIAGNOSTICS.json";
    let touches_spec = targets.iter().any(|path| *path == SPEC_FILE);
    let touches_lane = targets.iter().any(|path| is_lane_plan(path));
    let touches_master_plan = targets.iter().any(|path| *path == MASTER_PLAN_FILE);
    let touches_violations = targets.iter().any(|path| *path == VIOLATIONS_FILE);
    let touches_objectives = targets.iter().any(|path| *path == OBJECTIVES_FILE);
    let touches_diagnostics = targets
        .iter()
        .any(|path| *path == diagnostics_file || *path == legacy_diagnostics_file);
    let touches_src = targets.iter().any(|path| is_src_path(path));
    let touches_tests = targets.iter().any(|path| is_tests_path(path));
    let touches_other = targets.iter().any(|path| {
        *path != SPEC_FILE
            && *path != MASTER_PLAN_FILE
            && !is_src_path(path)
            && !is_tests_path(path)
            && !is_lane_plan(path)
            && *path != VIOLATIONS_FILE
            && *path != OBJECTIVES_FILE
            && *path != diagnostics_file
            && *path != legacy_diagnostics_file
    });
    PatchTargetTouches {
        diagnostics_file,
        legacy_diagnostics_file,
        touches_spec,
        touches_lane,
        touches_master_plan,
        touches_violations,
        touches_objectives,
        touches_diagnostics,
        touches_src,
        touches_tests,
        touches_other,
    }
}

fn executor_patch_scope_error(touches: &PatchTargetTouches, self_mod: bool) -> Option<String> {
    let spec_blocked = touches.touches_spec && !self_mod;
    let tests_blocked = touches.touches_tests && !self_mod;
    if spec_blocked
        || touches.touches_master_plan
        || touches.touches_lane
        || touches.touches_violations
        || touches.touches_diagnostics
        || tests_blocked
        || touches.touches_other
    {
        Some(
            "Executor may not patch plan files, violations, diagnostics, invariants, objectives, tests outside self-modification mode, or out-of-scope files. Execute code/tests only and report evidence in `message.payload`."
                .to_string(),
        )
    } else {
        None
    }
}

fn verifier_patch_scope_error(touches: &PatchTargetTouches) -> Option<String> {
    if touches.touches_violations {
        return Some(
            "apply_patch is ALWAYS rejected for VIOLATIONS.json. \
             Use the `violation` action instead — e.g. \
             {\"action\":\"violation\",\"op\":\"upsert\",\"violation\":{...}} to add/update or \
             {\"action\":\"violation\",\"op\":\"resolve\",\"violation_id\":\"<id>\"} to remove. \
             Retrying apply_patch on VIOLATIONS.json will produce this same error every time."
                .to_string(),
        );
    }
    if touches.touches_spec
        || touches.touches_lane
        || touches.touches_diagnostics
        || touches.touches_other
    {
        Some(
            "Verifier may not patch source files, SPEC.md, lane plans, or diagnostics. Use the `plan` action for PLAN.json updates and the `violation` action for VIOLATIONS.json."
                .to_string(),
        )
    } else {
        Some(
            "Verifier may not use apply_patch here. Use the `violation` action for VIOLATIONS.json and the `plan` action for PLAN.json."
                .to_string(),
        )
    }
}

fn planner_patch_scope_error(targets: &[&str], touches: &PatchTargetTouches) -> Option<String> {
    if touches.touches_master_plan {
        return Some(
            "apply_patch is ALWAYS rejected for PLAN.json. \
             Use the `plan` action instead — e.g. \
             {\"action\":\"plan\",\"op\":\"set_task_status\",\"task_id\":\"<id>\",\"status\":\"ready\",\
             \"rationale\":\"<why>\",\"predicted_next_actions\":[...]}. \
             Retrying apply_patch on PLAN.json will produce this same error every time."
                .to_string(),
        );
    }
    if touches.touches_spec
        || touches.touches_violations
        || targets
            .iter()
            .any(|path| is_src_path(path) || is_tests_path(path))
    {
        Some(
            "apply_patch on `src/`, `tests/`, `SPEC.md`, or `VIOLATIONS.json` is rejected for the planner role. \
             Planner does not write source code. \
             Use the `plan` action to create or update a task in PLAN.json and mark it `ready` so the executor picks it up. \
             Example: {\"action\":\"plan\",\"op\":\"create_task\",\"task\":{\"id\":\"<id>\",\"title\":\"<title>\",\
             \"status\":\"ready\",\"steps\":[\"...\"]},\"rationale\":\"<why>\",\"predicted_next_actions\":[...]}."
                .to_string(),
        )
    } else if touches.touches_lane || touches.touches_objectives {
        None
    } else {
        Some(
            "Planner may patch lane plans or `agent_state/OBJECTIVES.json` only. \
             Use the `plan` action for `PLAN.json` updates; no other patches are allowed."
                .to_string(),
        )
    }
}

fn diagnostics_patch_scope_error(touches: &PatchTargetTouches) -> Option<String> {
    if touches.touches_spec
        || touches.touches_master_plan
        || touches.touches_lane
        || touches.touches_violations
        || touches.touches_src
        || touches.touches_tests
        || touches.touches_other
    {
        Some(format!(
            "Diagnostics may only patch {} or {} because diagnostics owns ranked failure reporting.",
            touches.diagnostics_file,
            touches.legacy_diagnostics_file
        ))
    } else {
        None
    }
}

pub(crate) fn patch_scope_error(role: &str, patch: &str) -> Option<String> {
    patch_scope_error_with_mode(role, patch, is_self_modification_mode())
}

/// Walk up from `file_path` (workspace-relative) to find the nearest Cargo.toml.
/// Returns the package name from that manifest, or None if not found.
fn infer_crate_for_patch(workspace: &Path, file_path: &str) -> Option<String> {
    let mut dir = workspace.join(file_path);
    dir.pop(); // start from parent of the file
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            let text = std::fs::read_to_string(&manifest).ok()?;
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("name") {
                    let name = rest.trim().trim_start_matches('=').trim().trim_matches('"');
                    if !name.is_empty() {
                        return Some(name.to_string());
                    }
                }
            }
        }
        if dir == workspace {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

// ── Patch-anchor auto-read (mirrors harness_repair logic) ─────────────────────

const AUTO_READ_CONTEXT_BEFORE: usize = 20;
const AUTO_READ_CONTEXT_AFTER: usize = 40;

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_anchor_fail_path(err_msg: &str) -> Option<String> {
    let prefix = "Failed to find expected lines in ";
    err_msg.lines().find_map(|line| {
        line.strip_prefix(prefix)
            .map(|rest| rest.trim_end_matches(':').trim())
            .filter(|path| !path.is_empty())
            .map(str::to_string)
    })
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::vec::Vec<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_expected_anchor_lines(err_msg: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut capture = false;
    for line in err_msg.lines() {
        if !capture {
            capture = line.starts_with("Failed to find expected lines in ");
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || (!line.starts_with("    ") && !line.starts_with('\t')) {
            if !lines.is_empty() {
                break;
            }
            continue;
        }
        lines.push(trimmed.to_string());
    }
    lines
}

fn patch_failure_guidance(path: Option<&str>, err_msg: &str) -> String {
    let mut hints = Vec::new();
    hints.push(
        "Patch anchor miss: deleted/context lines must match the current file EXACTLY.".to_string(),
    );
    hints.push("Do not abbreviate deleted lines like `-1. Centralize d`; copy exact text from read_file output.".to_string());
    hints.push("Next step: emit `read_file` for the target file, then build a new patch with at least 3 unchanged context lines.".to_string());

    if let Some(file) = path {
        let diagnostics_file = diagnostics_file();
        let legacy_diagnostics_file = "DIAGNOSTICS.json";
        if file == diagnostics_file || file == legacy_diagnostics_file || file.ends_with(".md") {
            hints.push("This is a prose/markdown file: prefer rewriting the whole section or the whole file instead of a tiny surgical hunk.".to_string());
            hints.push(format!(
                "For {}, one full-file rewrite is usually more reliable than repeated partial patches.",
                diagnostics_file
            ));
        }
    }

    let anchors = extract_expected_anchor_lines(err_msg);
    if !anchors.is_empty() {
        hints.push(format!("Failed anchor lines: {}", anchors.join(" | ")));
    }

    hints.join("\n")
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str
/// Outputs: std::option::Option<(usize, usize, std::string::String)>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_anchor_context_excerpt(full: &str, err_msg: &str) -> Option<(usize, usize, String)> {
    let anchor_lines = extract_expected_anchor_lines(err_msg);
    if anchor_lines.is_empty() {
        return None;
    }
    let file_lines: Vec<&str> = full.lines().collect();
    let idx = anchor_lines
        .iter()
        .rev()
        .filter_map(|anchor| {
            let needle = anchor.trim();
            (needle.len() >= 8).then_some(needle)
        })
        .find_map(|needle| file_lines.iter().position(|line| line.contains(needle)))?;
    let start_idx = idx.saturating_sub(AUTO_READ_CONTEXT_BEFORE);
    let end_idx = (idx + AUTO_READ_CONTEXT_AFTER + 1).min(file_lines.len());
    let start_line = start_idx + 1;
    let excerpt = file_lines[start_idx..end_idx]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}: {}", start_line + i, l))
        .collect::<Vec<_>>()
        .join("\n");
    Some((start_line, end_idx, excerpt))
}

/// Auto-read the region near the failed anchor, falling back to the full file.
fn auto_read_for_patch_anchor(workspace: &Path, relative: &str, err_msg: &str) -> Result<String> {
    let path = safe_join(workspace, relative)?;
    let full = std::fs::read_to_string(&path)
        .with_context(|| format!("auto-read failed: {}", path.display()))?;
    if let Some((start, end, excerpt)) = extract_anchor_context_excerpt(&full, err_msg) {
        return Ok(format!("Current content near likely match of failed anchor in {relative} (lines {start}-{end}):\n{excerpt}"));
    }
    // Fallback: first MAX_FULL_READ_LINES lines of the file.
    let text = full
        .lines()
        .take(MAX_FULL_READ_LINES)
        .enumerate()
        .map(|(i, l)| format!("{}: {}", i + 1, l))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("Current content of {relative}:\n{text}"))
}

// ── Action executors ───────────────────────────────────────────────────────────

fn exec_list_dir(workspace: &Path, relative: &str) -> Result<String> {
    let path = safe_join(workspace, relative)?;
    let mut entries = std::fs::read_dir(&path)
        .with_context(|| format!("list_dir: {}", path.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    entries.sort();
    Ok(entries.join("\n"))
}

fn exec_read_file(
    workspace: &Path,
    relative: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String> {
    let path = safe_join(workspace, relative)?;
    let full =
        std::fs::read_to_string(&path).with_context(|| format!("read_file: {}", path.display()))?;
    let lines: Vec<&str> = full.lines().collect();
    let total = lines.len();
    let from = start_line.unwrap_or(1).saturating_sub(1).min(total);
    let max_lines = match (start_line, end_line) {
        (Some(start), Some(end)) if end >= start && end > 0 => {
            let end_idx = end.saturating_sub(1).min(total.saturating_sub(1));
            let start_idx = start.saturating_sub(1).min(total);
            end_idx.saturating_sub(start_idx).saturating_add(1)
        }
        (Some(_), Some(_)) => 0,
        (Some(_), None) => 500,
        (None, _) => MAX_FULL_READ_LINES,
    };
    let text = lines[from..]
        .iter()
        .take(max_lines)
        .enumerate()
        .map(|(i, l)| format!("{}: {}", from + i + 1, l))
        .collect::<Vec<_>>()
        .join("\n");
    let shown = max_lines.min(total.saturating_sub(from));
    if total > from + shown {
        Ok(format!(
            "{text}\n(file has {total} lines total; use \"line\":{} or \"line_end\" to read more)",
            from + shown + 1
        ))
    } else {
        Ok(text)
    }
}

fn handle_message_action(
    role: &str,
    step: usize,
    action: &Value,
    defer_planner_to_executor_handoff: bool,
    mut writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let status = action.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let payload = action
        .get("payload")
        .cloned()
        .unwrap_or_else(|| Value::Null);
    let summary = payload
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("message accepted");
    let full_message = serde_json::to_string_pretty(action).unwrap_or_else(|_| "{}".to_string());
    let msg_type = action.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let to_role = action.get("to").and_then(|v| v.as_str()).unwrap_or("");
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let _ = std::fs::create_dir_all(agent_state_dir);
    let normalized_role = role
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let normalized_to = to_role
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let planner_to_executor =
        normalized_role == "planner" && normalized_to.starts_with("executor");
    let executor_to_planner =
        normalized_to == "planner" && normalized_role.starts_with("executor");
    let _planner_executor_pair = planner_to_executor || executor_to_planner;
    // planner→executor handoffs must emit InboundMessageQueued + WakeSignalQueued.
    // The logged action path defers emission until after ActionResultRecorded so
    // the handoff causality invariant observes a strictly greater queue seq.
    // executor→planner completion messages go through app::persist_executor_completion_message
    // and are suppressed here to avoid double-routing.
    let persist_handoff_message = if planner_to_executor && defer_planner_to_executor_handoff {
        false
    } else {
        planner_to_executor
            || !executor_to_planner
            || status.eq_ignore_ascii_case("blocked")
            || msg_type.eq_ignore_ascii_case("blocker")
    };

    if normalized_role == normalized_to
        && !is_allowed_self_addressed_message(action, &normalized_role, &normalized_to)
    {
        return Ok((
            true,
            format!(
                "invalid self-addressed message suppressed: {normalized_role} -> {normalized_to}"
            ),
        ));
    }

    if let Some(result) = suppress_redundant_planner_blocker(
        role,
        msg_type,
        &payload,
        agent_state_dir,
        writer.as_deref_mut(),
    )
    {
        return Ok(result);
    }

    sync_verifier_blocker_state(
        role,
        to_role,
        msg_type,
        status,
        summary,
        &payload,
        agent_state_dir,
        writer.as_deref_mut(),
    );
    if persist_handoff_message {
        persist_inbound_message(
            role,
            step,
            std::path::Path::new(crate::constants::workspace()),
            action,
            &full_message,
            writer.as_deref_mut(),
        );
    }

    // Capture blocker messages as first-class artifact for invariant synthesis.
    if msg_type == "blocker" && status == "blocked" {
        let task_id = action.get("task_id").and_then(|v| v.as_str());
        let objective_id = action
            .get("objective_id")
            .or_else(|| payload.get("objective_id"))
            .and_then(|v| v.as_str());
        crate::blockers::record_blocker_message_with_writer(
            std::path::Path::new(crate::constants::workspace()),
            writer.as_deref_mut(),
            role,
            summary,
            task_id,
            objective_id,
        );
    }

    Ok((
        true,
        format!("{summary}\n\nmessage_action:\n{full_message}"),
    ))
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &str, &str, &serde_json::Value, &std::path::Path, std::option::Option<&mut canonical_writer::CanonicalWriter>
/// Outputs: std::option::Option<(bool, std::string::String)>
/// Effects: fs_read, fs_write, state_read, state_write, transitions_state
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn suppress_redundant_planner_blocker(
    role: &str,
    msg_type: &str,
    payload: &Value,
    agent_state_dir: &Path,
    mut writer: Option<&mut CanonicalWriter>,
) -> Option<(bool, String)> {
    if role != "planner" || msg_type != "blocker" {
        return None;
    }
    let evidence = payload
        .get("evidence")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if evidence.is_empty() {
        return None;
    }
    let evidence_hash = artifact_write_signature(&["planner_blocker_evidence", &evidence]);
    if let Some(w) = writer.as_deref_mut() {
        if w.state().planner_blocker_evidence_hash == evidence_hash {
            return Some((
                false,
                "Error executing action: planner blocker suppressed: evidence unchanged. \
                 Retry with materially updated evidence or choose a different action."
                    .to_string(),
            ));
        }
        w.apply(ControlEvent::PlannerBlockerEvidenceSet {
            evidence_hash: evidence_hash.clone(),
        });
    } else {
        let evidence_path = agent_state_dir.join("last_planner_blocker_evidence.txt");
        if let Ok(prev) = std::fs::read_to_string(&evidence_path) {
            if prev.trim() == evidence {
                return Some((
                    false,
                    "Error executing action: planner blocker suppressed: evidence unchanged. \
                     Retry with materially updated evidence or choose a different action."
                        .to_string(),
                ));
            }
        }
    }
    let evidence_path = agent_state_dir.join("last_planner_blocker_evidence.txt");
    let _ = std::fs::write(evidence_path, evidence);
    None
}

fn is_allowed_self_addressed_message(action: &Value, from_role: &str, to_role: &str) -> bool {
    from_role == "solo"
        && to_role == "solo"
        && action.get("action").and_then(|v| v.as_str()) == Some("message")
        && action
            .get("type")
            .and_then(|v| v.as_str())
            .is_some_and(|kind| kind.eq_ignore_ascii_case("result"))
        && action
            .get("status")
            .and_then(|v| v.as_str())
            .is_some_and(|status| status.eq_ignore_ascii_case("complete"))
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &str, &str, &str, &str, &str, &serde_json::Value, &std::path::Path, std::option::Option<&mut canonical_writer::CanonicalWriter>
/// Outputs: ()
/// Effects: fs_write, state_write, transitions_state
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn sync_verifier_blocker_state(
    role: &str,
    to_role: &str,
    msg_type: &str,
    status: &str,
    summary: &str,
    payload: &Value,
    agent_state_dir: &Path,
    mut writer: Option<&mut CanonicalWriter>,
) {
    if !to_role.eq_ignore_ascii_case("verifier") {
        return;
    }
    let active_path = agent_state_dir.join("active_blocker_to_verifier.json");
    if msg_type == "blocker" && status == "blocked" {
        if let Some(writer) = writer.as_deref_mut() {
            writer.apply(ControlEvent::VerifierBlockerSet { active: true });
        }
        let blocker_state = json!({
            "from": role,
            "summary": summary,
            "evidence": payload.get("evidence").and_then(|v| v.as_str()).unwrap_or(""),
            "required_action": payload.get("required_action").and_then(|v| v.as_str()).unwrap_or(""),
            "severity": payload.get("severity").and_then(|v| v.as_str()).unwrap_or(""),
        });
        let _ = std::fs::write(
            &active_path,
            serde_json::to_string_pretty(&blocker_state).unwrap_or_default(),
        );
    } else {
        if let Some(writer) = writer.as_deref_mut() {
            writer.apply(ControlEvent::VerifierBlockerSet { active: false });
        }
    }
    if active_path.exists() && !(msg_type == "blocker" && status == "blocked") {
        let _ = std::fs::remove_file(active_path);
    }
}

fn handle_list_dir_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let path = action
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("list_dir missing 'path'"))?;
    let out = exec_list_dir(workspace, path)?;
    Ok((false, format!("list_dir {path}:\n{out}")))
}

fn handle_read_file_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let path = action
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("read_file missing 'path'"))?;
    let line = action
        .get("line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let line_start = action
        .get("line_start")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let line_end = action
        .get("line_end")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let start = line_start.or(line);
    let out = exec_read_file(workspace, path, start, line_end)?;
    let receipt_id = append_evidence_receipt(
        role,
        step,
        "read_file",
        Some(path),
        Some(workspace.join(path)),
        json!({"line": line, "line_start": line_start, "line_end": line_end}),
        &out,
    )
    .ok();
    eprintln!(
        "[{role}] step={} read_file path={path} bytes={}",
        step,
        out.len()
    );
    Ok((
        false,
        format_output_with_evidence_receipt(
            &format!("read_file {path}"),
            &out,
            receipt_id.as_deref(),
        ),
    ))
}
