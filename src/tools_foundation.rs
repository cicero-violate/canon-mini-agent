/// Return a human-readable type name for a JSON value (used in validation error messages).
fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Extract the first file path touched by the patch (*** Update File: / *** Add File:).
fn patch_first_file(patch: &str) -> Option<&str> {
    patch.lines().find_map(|line| {
        line.strip_prefix("*** Update File:")
            .or_else(|| line.strip_prefix("*** Add File:"))
            .map(str::trim)
            .filter(|path| !path.is_empty())
    })
}

fn patch_targets<'a>(patch: &'a str) -> Vec<&'a str> {
    patch
        .lines()
        .filter_map(|line| {
            line.strip_prefix("*** Update File:")
                .or_else(|| line.strip_prefix("*** Add File:"))
                .or_else(|| line.strip_prefix("*** Delete File:"))
                .or_else(|| line.strip_prefix("*** Move to:"))
        })
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect()
}

fn patch_targets_include_rust_sources(paths: &[&str]) -> bool {
    paths.iter().any(|path| path.ends_with(".rs"))
}

fn rust_patch_verification_flag_path() -> PathBuf {
    Path::new(crate::constants::agent_state_dir()).join("rust_patch_verification_requested.flag")
}

fn request_rust_patch_verification_if_needed(
    patch_targets: &[&str],
    mut writer: Option<&mut CanonicalWriter>,
) {
    if !patch_targets_include_rust_sources(patch_targets) {
        return;
    }
    if let Some(w) = writer.as_deref_mut() {
        w.apply(ControlEvent::RustPatchVerificationRequested { requested: true });
    }
    let flag_path = rust_patch_verification_flag_path();
    if let Some(parent) = flag_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(flag_path, b"requested\n");
}

fn fnv1a64(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn artifact_write_signature(parts: &[&str]) -> String {
    fnv1a64(&parts.join("\u{1f}"))
}

fn file_snapshot(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn restore_file_snapshot(path: &Path, snapshot: &Option<Vec<u8>>) -> Result<()> {
    if let Some(bytes) = snapshot {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn emit_workspace_artifact_effect(
    writer: &mut CanonicalWriter,
    requested: bool,
    artifact: &str,
    op: &str,
    target: &str,
    subject: &str,
    signature: &str,
) -> Result<()> {
    let effect = if requested {
        crate::events::EffectEvent::WorkspaceArtifactWriteRequested {
            artifact: artifact.to_string(),
            op: op.to_string(),
            target: target.to_string(),
            subject: subject.to_string(),
            signature: signature.to_string(),
        }
    } else {
        crate::events::EffectEvent::WorkspaceArtifactWriteApplied {
            artifact: artifact.to_string(),
            op: op.to_string(),
            target: target.to_string(),
            subject: subject.to_string(),
            signature: signature.to_string(),
        }
    };
    writer.try_record_effect(effect)
}

fn try_emit_workspace_artifact_effect(
    writer: &mut CanonicalWriter,
    requested: bool,
    artifact: &str,
    op: &str,
    target: &str,
    subject: &str,
    signature: &str,
) -> Result<()> {
    emit_workspace_artifact_effect(writer, requested, artifact, op, target, subject, signature)
        .map_err(|err| {
            anyhow!(
                "canonical effect append failed for {} {} {}: {err:#}",
                artifact,
                op,
                subject
            )
        })
}

fn record_effect_for_workspace(workspace: &Path, effect: crate::events::EffectEvent) -> Result<()> {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let state = crate::system_state::SystemState::new(&[], 0);
    let mut writer = crate::canonical_writer::CanonicalWriter::try_new(
        state,
        crate::tlog::Tlog::open(&tlog_path),
        workspace.to_path_buf(),
    )?;
    writer.try_record_effect(effect)
}

fn write_projection_with_workspace_effects(
    workspace: &Path,
    path: &Path,
    artifact: &str,
    subject: &str,
    contents: &str,
) -> Result<()> {
    let signature =
        artifact_write_signature(&[artifact, "write", subject, &contents.len().to_string()]);
    let target = path.to_string_lossy().into_owned();
    crate::logging::record_workspace_artifact_effect(
        workspace, true, artifact, "write", &target, subject, &signature,
    )?;
    let snapshot = file_snapshot(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, contents)?;
    std::fs::rename(&tmp_path, path)?;
    if let Err(err) = crate::logging::record_workspace_artifact_effect(
        workspace, false, artifact, "write", &target, subject, &signature,
    ) {
        restore_file_snapshot(path, &snapshot)?;
        return Err(err);
    }
    Ok(())
}

fn is_lane_plan(path: &str) -> bool {
    if !path.starts_with("PLANS/") && !path.starts_with("agent_state/") {
        return false;
    }
    let is_json = path.ends_with(".json");
    let is_md = path.ends_with(".md");
    if !is_json && !is_md {
        return false;
    }
    // Allow both legacy and instance-scoped lane plans:
    // - PLANS/executor-<id>.json
    // - PLANS/<instance>/executor-<id>.json
    // - agent_state/<instance>/executor-<id>.json
    // - legacy .md variants
    path.starts_with("PLANS/executor-")
        || path.starts_with("agent_state/executor-")
        || path.contains("/executor-")
}

fn is_src_path(path: &str) -> bool {
    is_named_dir_path(path, "src")
}

fn is_tests_path(path: &str) -> bool {
    is_named_dir_path(path, "tests")
}

fn is_named_dir_path(path: &str, dir: &str) -> bool {
    path == dir || path.starts_with(&format!("{dir}/"))
}

fn default_graph_out_dir(workspace: &Path, crate_name: &str) -> PathBuf {
    workspace
        .join("state")
        .join("reports_out")
        .join("crates")
        .join(crate_name)
}

fn report_crate_dir(out_dir: &Path, crate_name: &str) -> PathBuf {
    let name_matches = out_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == crate_name)
        .unwrap_or(false);
    let parent_is_crates = out_dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|n| n == "crates")
        .unwrap_or(false);
    if name_matches && parent_is_crates {
        out_dir.to_path_buf()
    } else {
        out_dir.join("crates").join(crate_name)
    }
}

fn graph_artifact_edges(workspace: &Path, crate_name: &str) -> Option<(u64, u64)> {
    let path = workspace
        .join("state")
        .join("graph")
        .join("index")
        .join("by_crate")
        .join(format!("{crate_name}.json"));
    let raw = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let call = value
        .get("call_edge_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cfg = value
        .get("cfg_edge_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some((call, cfg))
}

fn resolve_graph_crate_name(workspace: &Path, crate_name: &str) -> Option<String> {
    let primary = crate_name.to_string();
    let alt = crate_name.replace('-', "_");
    let candidates = if primary == alt {
        vec![primary]
    } else {
        vec![primary, alt]
    };
    for name in candidates {
        if let Some((call, cfg)) = graph_artifact_edges(workspace, &name) {
            if call > 0 || cfg > 0 {
                return Some(name);
            }
        }
    }
    None
}

fn ensure_graph_artifact(
    workspace: &Path,
    crate_name: &str,
    role: &str,
    step: usize,
) -> Result<String> {
    if let Some(name) = resolve_graph_crate_name(workspace, crate_name) {
        return Ok(name);
    }
    // Try rustc wrapper build to refresh artifacts.
    let build_cmd = format!("cargo build -p {crate_name}");
    eprintln!(
        "[{role}] step={} graph_artifact build cmd={build_cmd}",
        step
    );
    let (build_ok, build_out) =
        exec_run_command(workspace, &build_cmd, crate::constants::workspace())?;
    eprintln!(
        "[{role}] step={} graph_artifact build {} output_bytes={}",
        step,
        if build_ok { "ok" } else { "failed" },
        build_out.len()
    );
    if build_ok {
        if let Some(name) = resolve_graph_crate_name(workspace, crate_name) {
            return Ok(name);
        }
    }
    bail!(
        "graph artifact missing or lacks call/cfg edges; build the crate with rustc-wrapper enabled (e.g. `cargo build -p {crate_name}`) to generate a capture artifact"
    )
}

fn read_first_lines(path: &Path, max_lines: usize, max_bytes: usize) -> Result<String> {
    let content = ctx_read(path)?;
    let mut out = String::new();
    for (idx, line) in content.lines().enumerate() {
        if idx >= max_lines || out.len() >= max_bytes {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

fn read_json_report(path: &Path, max_bytes: usize) -> Result<String> {
    let content = ctx_read(path)?;
    let trimmed = truncate(&content, max_bytes);
    Ok(trimmed.to_string())
}

fn normalize_objective_id_for_match(value: &str) -> String {
    value
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
}

fn objective_id_matches(candidate: &str, requested: &str) -> bool {
    normalize_objective_id_for_match(candidate) == normalize_objective_id_for_match(requested)
}

fn objective_compared_ids(objectives: &[crate::objectives::Objective]) -> Vec<String> {
    objectives.iter().map(|obj| obj.id.clone()).collect()
}

fn objective_compared_normalized_ids(objectives: &[crate::objectives::Objective]) -> Vec<String> {
    objectives
        .iter()
        .map(|obj| normalize_objective_id_for_match(&obj.id))
        .collect()
}
