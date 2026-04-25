/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<reports::ViolationsReport, anyhow::Error>
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_violations(path: &Path) -> Result<crate::reports::ViolationsReport> {
    let raw = fs::read_to_string(path).unwrap_or_default();
    if raw.trim().is_empty() {
        if let Some(workspace) = path.parent().and_then(|dir| dir.parent()) {
            return Ok(crate::reports::load_violations_report(workspace));
        }
        return Ok(crate::reports::ViolationsReport {
            status: "ok".to_string(),
            summary: String::new(),
            violations: vec![],
        });
    }
    serde_json::from_str(&raw).map_err(|e| anyhow!("VIOLATIONS.json parse error: {e}"))
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &reports::ViolationsReport, std::option::Option<&mut canonical_writer::CanonicalWriter>, &str, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: fs_write, logging, state_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn save_violations(
    path: &Path,
    report: &crate::reports::ViolationsReport,
    mut writer: Option<&mut CanonicalWriter>,
    op: &str,
    subject: &str,
) -> Result<()> {
    let json = serde_json::to_string_pretty(report)?;
    let effect = crate::events::EffectEvent::ViolationsReportRecorded {
        report: report.clone(),
    };
    if let Some(writer_ref) = writer.as_deref_mut() {
        writer_ref.try_record_effect(effect)?;
    } else {
        crate::logging::record_effect_for_workspace(
            std::path::Path::new(crate::constants::workspace()),
            effect,
        )?;
    }
    crate::logging::write_projection_with_artifact_effects(
        std::path::Path::new(crate::constants::workspace()),
        path,
        VIOLATIONS_FILE,
        op,
        subject,
        &json,
    )
    .map_err(|e| anyhow!("failed to write VIOLATIONS.json: {e}"))
}

/// Intent: diagnostic_scan
/// Resource: violations_report_view
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: reads violations report from path
/// Forbidden: mutation
/// Invariants: returns pretty-printed loaded violations report with non-fatal flag false
/// Failure: returns load or JSON serialization errors
/// Provenance: rustc:facts + rustc:docstring
fn read_violation_report(path: &Path) -> Result<(bool, String)> {
    let report = load_violations(path)?;
    Ok((false, serde_json::to_string_pretty(&report)?))
}

fn upsert_violation(
    mut writer: Option<&mut CanonicalWriter>,
    path: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    use crate::reports::Violation;

    let lease = validate_evidence_lease(action)?;
    let v_val = action
        .get("violation")
        .ok_or_else(|| anyhow!("violation upsert requires a 'violation' object"))?;
    let mut violation: Violation =
        serde_json::from_value(v_val.clone()).map_err(|e| anyhow!("invalid violation payload: {e}"))?;
    if violation.id.trim().is_empty() {
        bail!("violation.id must be non-empty");
    }
    apply_violation_freshness(&mut violation, &lease);

    let mut report = load_violations(path)?;
    let result = if let Some(existing) = report.violations.iter_mut().find(|item| item.id == violation.id) {
        *existing = violation.clone();
        format!("violation upsert ok — updated `{}`", violation.id)
    } else {
        report.violations.push(violation.clone());
        format!("violation upsert ok — added `{}`", violation.id)
    };

    save_violations(path, &report, writer.as_deref_mut(), "upsert", &violation.id)?;
    Ok((false, result))
}

fn resolve_violation(
    mut writer: Option<&mut CanonicalWriter>,
    path: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    validate_evidence_lease(action)?;
    let violation_id = action
        .get("violation_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("violation resolve requires 'violation_id'"))?;
    let mut report = load_violations(path)?;
    let before = report.violations.len();
    report.violations.retain(|violation| violation.id != violation_id);
    if report.violations.len() == before {
        bail!("violation not found: {violation_id}");
    }
    if report.violations.is_empty() {
        report.status = "ok".to_string();
    }
    save_violations(path, &report, writer.as_deref_mut(), "resolve", violation_id)?;
    Ok((
        false,
        format!("violation resolve ok — removed `{violation_id}`"),
    ))
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: std::option::Option<&mut canonical_writer::CanonicalWriter>, &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn set_violation_status(
    mut writer: Option<&mut CanonicalWriter>,
    path: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    validate_evidence_lease(action)?;
    let status = action
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("violation set_status requires 'status'"))?;
    let mut report = load_violations(path)?;
    report.status = status.to_string();
    if let Some(summary) = action.get("summary").and_then(|v| v.as_str()) {
        report.summary = summary.to_string();
    }
    save_violations(path, &report, writer.as_deref_mut(), "set_status", status)?;
    Ok((
        false,
        format!("violation set_status ok — status=`{status}`"),
    ))
}

fn replace_violations(
    mut writer: Option<&mut CanonicalWriter>,
    path: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    use crate::reports::ViolationsReport;

    let lease = validate_evidence_lease(action)?;
    let report_value = action
        .get("report")
        .ok_or_else(|| anyhow!("violation replace requires a 'report' object"))?;
    let mut report: ViolationsReport = serde_json::from_value(report_value.clone())
        .map_err(|e| anyhow!("invalid ViolationsReport payload: {e}"))?;
    report.violations.retain(violation_is_fresh);
    for violation in &mut report.violations {
        apply_violation_freshness(violation, &lease);
    }
    save_violations(path, &report, writer.as_deref_mut(), "replace", "report")?;
    Ok((
        false,
        format!("violation replace ok — {} violation(s)", report.violations.len()),
    ))
}

fn handle_violation_action(
    writer: Option<&mut CanonicalWriter>,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let op_raw = action.get("op").and_then(|v| v.as_str()).unwrap_or("read");
    let path = workspace.join(VIOLATIONS_FILE);

    match op_raw {
        "read" => read_violation_report(&path),
        "upsert" => upsert_violation(writer, &path, action),
        "resolve" => resolve_violation(writer, &path, action),
        "set_status" => set_violation_status(writer, &path, action),
        "replace" => replace_violations(writer, &path, action),
        _ => bail!(
            "unknown violation op '{op_raw}' — use: read | upsert | resolve | set_status | replace"
        ),
    }
}

fn handle_issue_action(
    mut writer: Option<&mut CanonicalWriter>,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    if let Err(err) = crate::issues::sweep_stale_issues(workspace) {
        eprintln!("[issue] stale sweep failed: {err:#}");
    }
    let op_raw = action.get("op").and_then(|v| v.as_str()).unwrap_or("read");
    let path = workspace.join(ISSUES_FILE);
    let raw = serde_json::to_string_pretty(&crate::issues::load_issues_file(workspace))
        .unwrap_or_default();
    match op_raw {
        "read" => read_open_issues(&raw),
        "create" => create_issue(action, &path, &raw, writer.as_deref_mut()),
        "update" => update_issue(action, &path, &raw, writer.as_deref_mut()),
        "delete" => delete_issue(action, &path, &raw, writer.as_deref_mut()),
        "set_status" => set_issue_status(action, &path, &raw, writer.as_deref_mut()),
        "upsert" => upsert_issue(action, &path, &raw, writer.as_deref_mut()),
        "resolve" => resolve_issue(action, &path, &raw, writer.as_deref_mut()),
        _ => {
            bail!(
                "unknown issue op '{op_raw}' — use read | create | update | delete | set_status | upsert | resolve"
            )
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EvidenceReceipt {
    id: String,
    ts_ms: u64,
    actor: String,
    step: usize,
    action: String,
    path: Option<String>,
    abs_path: Option<String>,
    meta: Value,
    output_hash: String,
}

#[derive(Debug, Clone)]
struct EvidenceLease {
    receipt_ids: Vec<String>,
    validated_from: Vec<String>,
    evidence_hashes: Vec<String>,
    last_validated_ms: u64,
}

fn evidence_receipts_path() -> PathBuf {
    Path::new(crate::constants::agent_state_dir()).join("evidence_receipts.jsonl")
}

fn stable_hash_hex(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Intent: event_append
/// Resource: error
/// Inputs: &str, usize, &str, std::option::Option<&str>, std::option::Option<std::path::PathBuf>, serde_json::Value, &str
/// Outputs: std::result::Result<std::string::String, anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn append_evidence_receipt(
    role: &str,
    step: usize,
    action: &str,
    rel_path: Option<&str>,
    abs_path: Option<PathBuf>,
    meta: Value,
    output: &str,
) -> Result<String> {
    let ts_ms = now_ms();
    let id = format!("rcpt-{ts_ms}-{role}-{step}-{action}");
    let receipt = build_evidence_receipt(
        &id,
        ts_ms,
        role,
        step,
        action,
        rel_path,
        abs_path,
        meta,
        output,
    );
    let path = evidence_receipts_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", serde_json::to_string(&receipt)?)?;
    Ok(id)
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, u64, &str, usize, &str, std::option::Option<&str>, std::option::Option<std::path::PathBuf>, serde_json::Value, &str
/// Outputs: tools::EvidenceReceipt
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_evidence_receipt(
    id: &str,
    ts_ms: u64,
    role: &str,
    step: usize,
    action: &str,
    rel_path: Option<&str>,
    abs_path: Option<PathBuf>,
    meta: Value,
    output: &str,
) -> EvidenceReceipt {
    EvidenceReceipt {
        id: id.to_string(),
        ts_ms,
        actor: role.to_string(),
        step,
        action: action.to_string(),
        path: rel_path.map(|s| s.to_string()),
        abs_path: abs_path.map(|p| p.display().to_string()),
        meta,
        output_hash: stable_hash_hex(output),
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, std::option::Option<&str>
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn format_output_with_evidence_receipt(
    prefix: &str,
    out: &str,
    receipt_id: Option<&str>,
) -> String {
    match receipt_id {
        Some(receipt_id) => format!("{prefix}:\nEvidence receipt: {receipt_id}\n{out}",),
        None => format!("{prefix}:\n{out}"),
    }
}

/// Intent: validation_gate
/// Resource: error
/// Inputs: &serde_json::Value
/// Outputs: std::result::Result<tools::EvidenceLease, anyhow::Error>
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn validate_evidence_lease(action: &Value) -> Result<EvidenceLease> {
    let receipt_ids = action
        .get("evidence_receipts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("authoritative mutation requires non-empty 'evidence_receipts'"))?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect::<Vec<_>>();
    if receipt_ids.is_empty() {
        bail!("authoritative mutation requires non-empty 'evidence_receipts'");
    }
    let raw = fs::read_to_string(evidence_receipts_path()).unwrap_or_default();
    let mut validated_from = Vec::new();
    let mut evidence_hashes = Vec::new();
    let mut last_validated_ms = 0u64;
    for receipt_id in &receipt_ids {
        let maybe_receipt = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<EvidenceReceipt>(line).ok())
            .find(|receipt| &receipt.id == receipt_id);
        let Some(receipt) = maybe_receipt else {
            bail!("evidence receipt not found: {receipt_id}");
        };
        if now_ms().saturating_sub(receipt.ts_ms) > 15 * 60 * 1000 {
            bail!("evidence receipt is stale: {receipt_id}");
        }
        if let Some(path) = receipt.path.or(receipt.abs_path) {
            validated_from.push(path);
        }
        evidence_hashes.push(receipt.output_hash);
        last_validated_ms = last_validated_ms.max(receipt.ts_ms);
    }
    validated_from.sort();
    validated_from.dedup();
    evidence_hashes.sort();
    evidence_hashes.dedup();
    Ok(EvidenceLease {
        receipt_ids,
        validated_from,
        evidence_hashes,
        last_validated_ms,
    })
}

fn apply_issue_freshness(issue: &mut Issue, lease: &EvidenceLease) {
    issue.freshness_status = "fresh".to_string();
    issue.stale_reason.clear();
    issue.last_validated_ms = lease.last_validated_ms;
    issue.validated_from = lease.validated_from.clone();
    issue.evidence_receipts = lease.receipt_ids.clone();
    issue.evidence_hashes = lease.evidence_hashes.clone();
}

fn violation_is_fresh(violation: &crate::reports::Violation) -> bool {
    match violation
        .freshness_status
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "fresh" => true,
        "stale" | "unknown" => false,
        _ => violation.last_validated_ms > 0,
    }
}

fn apply_violation_freshness(violation: &mut crate::reports::Violation, lease: &EvidenceLease) {
    violation.freshness_status = "fresh".to_string();
    violation.stale_reason.clear();
    violation.last_validated_ms = lease.last_validated_ms;
    violation.validated_from = lease.validated_from.clone();
    violation.evidence_receipts = lease.receipt_ids.clone();
    violation.evidence_hashes = lease.evidence_hashes.clone();
}

/// Intent: diagnostic_scan
/// Resource: open_issues_view
/// Inputs: &str
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: none
/// Forbidden: mutation
/// Invariants: returns no-open marker for empty, closed-only, or stale-only issue input; otherwise returns pretty filtered open fresh issues
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
fn read_open_issues(raw: &str) -> Result<(bool, String)> {
    if raw.trim().is_empty() {
        return Ok((false, "(no open issues)".to_string()));
    }
    let mut file: IssuesFile = serde_json::from_str(raw).unwrap_or_default();
    file.issues.retain(|i| !is_closed(i));
    file.issues.retain(crate::issues::issue_is_fresh);
    if file.issues.is_empty() {
        return Ok((false, "(no open issues)".to_string()));
    }
    Ok((
        false,
        serde_json::to_string_pretty(&file).unwrap_or(raw.to_string()),
    ))
}

fn create_issue(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let lease = validate_evidence_lease(action)?;
    let issue_val = action
        .get("issue")
        .ok_or_else(|| anyhow!("issue create missing 'issue' field"))?;

    let mut file = parse_issues_file_allow_empty(raw)?;
    let mut issue = parse_issue_payload(issue_val)?;
    apply_issue_freshness(&mut issue, &lease);
    let issue_id = issue.id.clone();
    if file.issues.iter().any(|i| i.id == issue_id) {
        bail!("issue id already exists: {}", issue_id);
    }
    file.issues.push(issue);
    write_issues_file(path, &mut file, writer, "create", &issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, "issue create ok".to_string()))
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value
/// Outputs: std::result::Result<issues::Issue, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_issue_payload(issue_val: &Value) -> Result<Issue> {
    // Pre-check: collect all missing required string fields before serde attempts deserialization.
    // serde only reports the first missing field; this lists them all so the LLM can fix in one shot.
    if let Some(obj) = issue_val.as_object() {
        let required_string_fields = ["id", "title", "status", "priority"];
        let missing: Vec<&str> = required_string_fields
            .iter()
            .copied()
            .filter(|&field| {
                obj.get(field)
                    .map(|v| v.as_str().map(|s| s.trim().is_empty()).unwrap_or(true))
                    .unwrap_or(true)
            })
            .collect();
        if !missing.is_empty() {
            bail!(
                "invalid issue payload: missing required fields: {}. Required: {{\"id\":\"<id>\",\"title\":\"<title>\",\"status\":\"open\",\"priority\":\"medium\",\"kind\":\"<kind>\",\"description\":\"<description>\"}}",
                missing.join(", ")
            );
        }
        // Pre-check: field type validation — ensure string fields are not wrong types.
        let string_fields = [
            "id",
            "title",
            "status",
            "priority",
            "kind",
            "description",
            "location",
            "discovered_by",
        ];
        for field in &string_fields {
            if let Some(v) = obj.get(*field) {
                if !v.is_string() && !v.is_null() {
                    bail!(
                        "invalid issue payload: field '{}' must be a string, got {}",
                        field,
                        value_type_name(v)
                    );
                }
            }
        }
    } else if !issue_val.is_null() {
        bail!(
            "invalid issue payload: expected an object, got {}",
            value_type_name(issue_val)
        );
    }

    let issue: Issue = serde_json::from_value(issue_val.clone())
        .map_err(|e| anyhow!("invalid issue payload: {e}"))?;
    if issue.id.trim().is_empty() {
        bail!("issue.id must be non-empty");
    }
    Ok(issue)
}

fn upsert_issue(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let lease = validate_evidence_lease(action)?;
    let issue_val = action
        .get("issue")
        .ok_or_else(|| anyhow!("issue upsert missing 'issue' field"))?;
    let mut file = parse_issues_file_allow_empty(raw)?;
    let mut issue = parse_issue_payload(issue_val)?;
    if let Some(issue_id) = action.get("issue_id").and_then(|v| v.as_str()) {
        if issue.id != issue_id {
            bail!(
                "issue upsert mismatch: issue_id '{}' does not match issue.id '{}'",
                issue_id,
                issue.id
            );
        }
    }
    apply_issue_freshness(&mut issue, &lease);
    let issue_id = issue.id.clone();
    let outcome = if let Some(existing) = file.issues.iter_mut().find(|i| i.id == issue_id) {
        *existing = issue;
        "updated"
    } else {
        file.issues.push(issue);
        "added"
    };
    write_issues_file(path, &mut file, writer, "upsert", &issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, format!("issue upsert ok — {outcome} `{issue_id}`")))
}

fn resolve_issue(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let lease = validate_evidence_lease(action)?;
    let issue_id = action
        .get("issue_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            action
                .get("issue")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
        })
        .ok_or_else(|| anyhow!("issue resolve missing 'issue_id'"))?;
    let mut file = parse_issues_file_allow_empty(raw)?;
    let Some(issue) = file.issues.iter_mut().find(|i| i.id == issue_id) else {
        return Ok((false, format!("issue resolve ok — `{issue_id}` (already absent)")));
    };
    issue.status = "resolved".to_string();
    apply_issue_freshness(issue, &lease);
    write_issues_file(path, &mut file, writer, "resolve", issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, format!("issue resolve ok — `{issue_id}`")))
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &serde_json::Value, &std::path::Path, &str, std::option::Option<&mut canonical_writer::CanonicalWriter>
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn update_issue(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let lease = validate_evidence_lease(action)?;
    let issue_id = action
        .get("issue_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("issue update missing 'issue_id'"))?;
    let updates = action
        .get("updates")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("issue update missing 'updates' object"))?;
    let mut file = parse_issues_file_allow_empty(raw)?;
    let mut issue = if let Some(existing) = file.issues.iter().find(|i| i.id == issue_id) {
        existing.clone()
    } else {
        synthesize_issue_stub(issue_id, None)
    };
    let mut value = serde_json::to_value(issue.clone())?;
    if let Some(map) = value.as_object_mut() {
        for (k, v) in updates {
            map.insert(k.clone(), v.clone());
        }
    }
    issue = serde_json::from_value(value)?;
    apply_issue_freshness(&mut issue, &lease);
    if let Some(existing) = file.issues.iter_mut().find(|i| i.id == issue_id) {
        *existing = issue;
    } else {
        file.issues.push(issue);
    }
    write_issues_file(path, &mut file, writer, "update", issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, "issue update ok".to_string()))
}

fn delete_issue(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let issue_id = action
        .get("issue_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("issue delete missing 'issue_id'"))?;
    let mut file: IssuesFile = serde_json::from_str(raw).unwrap_or_default();
    let before = file.issues.len();
    file.issues.retain(|i| i.id != issue_id);
    if file.issues.len() == before {
        bail!("issue not found: {issue_id}");
    }
    write_issues_file(path, &mut file, writer, "delete", issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, "issue delete ok".to_string()))
}

/// Intent: canonical_write
/// Resource: issue_status
/// Inputs: &serde_json::Value, &std::path::Path, &str, std::option::Option<&mut canonical_writer::CanonicalWriter>
/// Outputs: std::result::Result<(bool, std::string::String), anyhow::Error>
/// Effects: updates or creates issue status, applies freshness lease, writes issues file, and queues diagnostics reconciliation
/// Forbidden: issue status mutation without evidence lease validation
/// Invariants: existing issue receives requested status; missing issue is synthesized with requested status before persistence
/// Failure: returns missing field, lease validation, parse, or write errors
/// Provenance: rustc:facts + rustc:docstring
fn set_issue_status(
    action: &Value,
    path: &Path,
    raw: &str,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(bool, String)> {
    let lease = validate_evidence_lease(action)?;
    let issue_id = action
        .get("issue_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("issue set_status missing 'issue_id'"))?;
    let status = action
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("issue set_status missing 'status'"))?;
    let mut file = parse_issues_file_allow_empty(raw)?;
    if let Some(issue) = file.issues.iter_mut().find(|i| i.id == issue_id) {
        issue.status = status.to_string();
        apply_issue_freshness(issue, &lease);
    } else {
        let mut issue = synthesize_issue_stub(issue_id, Some(status));
        apply_issue_freshness(&mut issue, &lease);
        file.issues.push(issue);
    }
    write_issues_file(path, &mut file, writer, "set_status", issue_id)?;
    queue_diagnostics_reconciliation();
    Ok((false, "issue set_status ok".to_string()))
}

fn queue_diagnostics_reconciliation() {
    // Wake signals are canonicalized through ControlEvent::WakeSignalQueued.
    // Legacy wakeup_*.flag projections are retired.
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::result::Result<issues::IssuesFile, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_issues_file_allow_empty(raw: &str) -> Result<IssuesFile> {
    if raw.trim().is_empty() {
        Ok(IssuesFile::default())
    } else {
        parse_issues_file_required(raw)
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::result::Result<issues::IssuesFile, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_issues_file_required(raw: &str) -> Result<IssuesFile> {
    serde_json::from_str(raw).map_err(|e| anyhow!("failed to parse ISSUES.json: {e}"))
}

fn synthesize_issue_stub(issue_id: &str, status: Option<&str>) -> Issue {
    let title = issue_id
        .trim()
        .trim_start_matches("auto_")
        .replace('_', " ")
        .trim()
        .to_string();
    Issue {
        id: issue_id.to_string(),
        title: if title.is_empty() {
            issue_id.to_string()
        } else {
            title
        },
        status: status.unwrap_or("open").to_string(),
        priority: "medium".to_string(),
        kind: "stale_state".to_string(),
        description: format!(
            "Auto-created issue stub for missing issue id `{issue_id}` during runtime synchronization."
        ),
        discovered_by: "planner".to_string(),
        freshness_status: "unknown".to_string(),
        ..Issue::default()
    }
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &mut issues::IssuesFile, std::option::Option<&mut canonical_writer::CanonicalWriter>, &str, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn write_issues_file(
    path: &Path,
    file: &mut IssuesFile,
    writer: Option<&mut CanonicalWriter>,
    op: &str,
    subject: &str,
) -> Result<()> {
    crate::issues::rescore_all(file);
    let workspace = path
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| std::path::Path::new(crate::constants::workspace()));
    crate::issues::persist_issues_projection_with_writer(workspace, file, writer, subject)
        .map_err(|e| anyhow!("failed to write ISSUES.json via {op}: {e}"))
}
