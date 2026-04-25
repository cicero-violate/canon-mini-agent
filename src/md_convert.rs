use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::constants::{INVARIANTS_FILE, INVARIANTS_MD_FILE, OBJECTIVES_FILE, OBJECTIVES_MD_FILE};
use crate::reports::{
    Invariant, InvariantCategory, InvariantLevel, InvariantsReport, Objective, ObjectiveCategory,
    ObjectiveLevel, ObjectivesReport,
};

/// Intent: repair_or_initialize
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: fs_read, state_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn ensure_objectives_and_invariants_json(workspace: &Path) -> Result<()> {
    let invariants_md = workspace.join(INVARIANTS_MD_FILE);
    let objectives_md = workspace.join(OBJECTIVES_MD_FILE);

    if invariants_md.exists() {
        let text = std::fs::read_to_string(&invariants_md).unwrap_or_default();
        let report = parse_invariants_md(&text);
        let invariants_json = workspace.join(INVARIANTS_FILE);
        write_json(&invariants_json, &report)?;
    }

    if objectives_md.exists() {
        let text = std::fs::read_to_string(&objectives_md).unwrap_or_default();
        let report = parse_objectives_md(&text);
        let objectives_json = workspace.join(OBJECTIVES_FILE);
        write_json(&objectives_json, &report)?;
    }

    Ok(())
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::PathBuf, &impl serde::Serialize
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn write_json(path: &PathBuf, value: &impl serde::Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(value)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: reports::InvariantsReport
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_invariants_md(text: &str) -> InvariantsReport {
    let mut invariants: Vec<Invariant> = Vec::new();
    let mut principles: Vec<String> = Vec::new();
    let mut math: Vec<String> = Vec::new();
    let mut meta: Vec<String> = Vec::new();

    let mut current_title: Option<String> = None;
    let mut current_clauses: Vec<String> = Vec::new();
    let mut current_desc: Vec<String> = Vec::new();
    let mut section = InvariantSection::Body;

    fn push_current_invariant(
        title_opt: &mut Option<String>,
        clauses: &mut Vec<String>,
        desc: &mut Vec<String>,
        out: &mut Vec<Invariant>,
    ) {
        if let Some(title) = title_opt.take() {
            let description = desc.join(" ").trim().to_string();
            let invariant = Invariant {
                id: slugify(&title),
                title,
                category: categorize_invariant(&description),
                level: InvariantLevel::Medium,
                description,
                clauses: std::mem::take(clauses),
            };
            out.push(invariant);
            desc.clear();
        }
    }

    fn handle_invariant_h2_heading(
        title: &str,
        current_title: &mut Option<String>,
        current_clauses: &mut Vec<String>,
        current_desc: &mut Vec<String>,
        invariants: &mut Vec<Invariant>,
        meta: &mut Vec<String>,
        section: &mut InvariantSection,
    ) -> bool {
        if title.eq_ignore_ascii_case("math") {
            push_current_invariant(current_title, current_clauses, current_desc, invariants);
            *section = InvariantSection::Math;
            return true;
        }
        if title.starts_with("Additional Exhaustive") {
            push_current_invariant(current_title, current_clauses, current_desc, invariants);
            *section = InvariantSection::Body;
            return true;
        }
        if title.starts_with("Meta-Level")
            || title.starts_with("Insight")
            || title.starts_with("Final")
        {
            push_current_invariant(current_title, current_clauses, current_desc, invariants);
            *section = InvariantSection::Meta;
            if !title.is_empty() {
                meta.push(title.to_string());
            }
            return true;
        }
        false
    }

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("## ") {
            let title = line.trim_start_matches("## ").trim();
            if handle_invariant_h2_heading(
                title,
                &mut current_title,
                &mut current_clauses,
                &mut current_desc,
                &mut invariants,
                &mut meta,
                &mut section,
            ) {
                continue;
            }
        }
        if line.starts_with("### ") {
            push_current_invariant(
                &mut current_title,
                &mut current_clauses,
                &mut current_desc,
                &mut invariants,
            );
            current_title = Some(line.trim_start_matches("### ").trim().to_string());
            section = InvariantSection::Body;
            continue;
        }
        if line.ends_with("invariant") && !line.starts_with("-") && !line.starts_with("*") {
            let title = line.to_string();
            invariants.push(Invariant {
                id: slugify(&title),
                title,
                category: InvariantCategory::Other,
                level: InvariantLevel::High,
                description: String::new(),
                clauses: Vec::new(),
            });
            continue;
        }
        if line.starts_with("A closed-loop invariant system means") {
            section = InvariantSection::Principles;
            continue;
        }
        match section {
            InvariantSection::Math => {
                math.push(strip_bullet(line));
            }
            InvariantSection::Meta => {
                meta.push(strip_bullet(line));
            }
            InvariantSection::Principles => {
                principles.push(strip_bullet(line));
            }
            InvariantSection::Body => {
                if line.starts_with("- ") || line.starts_with("* ") {
                    current_clauses.push(strip_bullet(line));
                } else if current_title.is_some() {
                    current_desc.push(line.to_string());
                }
            }
        }
    }
    push_current_invariant(
        &mut current_title,
        &mut current_clauses,
        &mut current_desc,
        &mut invariants,
    );

    InvariantsReport {
        version: 1,
        invariants,
        principles,
        math,
        meta,
    }
}

#[derive(Clone, Copy)]
enum InvariantSection {
    Body,
    Principles,
    Math,
    Meta,
}

/// Intent: pure_transform
/// Resource: objectives_markdown
/// Inputs: &str
/// Outputs: reports::ObjectivesReport
/// Effects: none
/// Forbidden: mutation
/// Invariants: parses objective headings and recognized sections into versioned ObjectivesReport, finalizing the current objective at EOF
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
fn parse_objectives_md(text: &str) -> ObjectivesReport {
    let mut objectives: Vec<Objective> = Vec::new();
    let mut goal: Vec<String> = Vec::new();
    let mut instrumentation: Vec<String> = Vec::new();
    let mut definition_of_done: Vec<String> = Vec::new();
    let mut non_goals: Vec<String> = Vec::new();

    let mut current: Option<ObjectiveBuilder> = None;
    let mut section = ObjectiveSection::None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("## ") {
            if handle_objectives_h2_heading(line, &mut objectives, &mut current, &mut section) {
                continue;
            }
        }
        if line.starts_with("### ") {
            section = objective_h3_section(line);
            continue;
        }
        push_objective_section_line(
            line,
            section,
            &mut current,
            &mut goal,
            &mut instrumentation,
            &mut definition_of_done,
            &mut non_goals,
        );
    }
    finalize_current_objective(&mut objectives, &mut current);

    ObjectivesReport {
        version: 1,
        objectives,
        goal,
        instrumentation,
        definition_of_done,
        non_goals,
    }
}

fn handle_objectives_h2_heading(
    line: &str,
    objectives: &mut Vec<Objective>,
    current: &mut Option<ObjectiveBuilder>,
    section: &mut ObjectiveSection,
) -> bool {
    let title = line.trim_start_matches("## ").trim();
    if let Some(next_section) = objective_h2_section(title) {
        *section = next_section;
        finalize_current_objective(objectives, current);
        return true;
    }
    if title.contains("Objective") || title.starts_with("OBJ-") {
        finalize_current_objective(objectives, current);
        *current = Some(ObjectiveBuilder::new(title));
        *section = ObjectiveSection::None;
        return true;
    }
    false
}

fn objective_h2_section(title: &str) -> Option<ObjectiveSection> {
    if title.contains("Goal") {
        Some(ObjectiveSection::Goal)
    } else if title.contains("Instrumentation") {
        Some(ObjectiveSection::Instrumentation)
    } else if title.contains("Definition of Done") {
        Some(ObjectiveSection::DefinitionDone)
    } else if title.contains("Non-Goals") {
        Some(ObjectiveSection::NonGoals)
    } else {
        None
    }
}

fn objective_h3_section(line: &str) -> ObjectiveSection {
    let title = line.trim_start_matches("### ").trim().to_lowercase();
    match title.as_str() {
        "requirement" => ObjectiveSection::Requirement,
        "verification" => ObjectiveSection::Verification,
        "success criteria" => ObjectiveSection::SuccessCriteria,
        _ => ObjectiveSection::None,
    }
}

fn push_objective_section_line(
    line: &str,
    section: ObjectiveSection,
    current: &mut Option<ObjectiveBuilder>,
    goal: &mut Vec<String>,
    instrumentation: &mut Vec<String>,
    definition_of_done: &mut Vec<String>,
    non_goals: &mut Vec<String>,
) {
    match section {
        ObjectiveSection::Goal => goal.push(strip_bullet(line)),
        ObjectiveSection::Instrumentation => instrumentation.push(strip_bullet(line)),
        ObjectiveSection::DefinitionDone => definition_of_done.push(strip_bullet(line)),
        ObjectiveSection::NonGoals => non_goals.push(strip_bullet(line)),
        ObjectiveSection::Requirement => {
            push_current_objective_line(current, line, ObjectiveField::Requirement)
        }
        ObjectiveSection::Verification => {
            push_current_objective_line(current, line, ObjectiveField::Verification)
        }
        ObjectiveSection::SuccessCriteria => {
            push_current_objective_line(current, line, ObjectiveField::SuccessCriteria)
        }
        ObjectiveSection::None => {
            push_current_objective_line(current, line, ObjectiveField::Description)
        }
    }
}

#[derive(Clone, Copy)]
enum ObjectiveField {
    Description,
    Requirement,
    Verification,
    SuccessCriteria,
}

fn push_current_objective_line(
    current: &mut Option<ObjectiveBuilder>,
    line: &str,
    field: ObjectiveField,
) {
    if let Some(obj) = current.as_mut() {
        match field {
            ObjectiveField::Description => obj.description.push(line.to_string()),
            ObjectiveField::Requirement => obj.requirement.push(strip_bullet(line)),
            ObjectiveField::Verification => obj.verification.push(strip_bullet(line)),
            ObjectiveField::SuccessCriteria => obj.success_criteria.push(strip_bullet(line)),
        }
    }
}

fn finalize_current_objective(
    objectives: &mut Vec<Objective>,
    current: &mut Option<ObjectiveBuilder>,
) {
    if let Some(obj) = current.take() {
        objectives.push(obj.finish());
    }
}

struct ObjectiveBuilder {
    title: String,
    category: ObjectiveCategory,
    level: ObjectiveLevel,
    description: Vec<String>,
    requirement: Vec<String>,
    verification: Vec<String>,
    success_criteria: Vec<String>,
}

impl ObjectiveBuilder {
    fn new(title: &str) -> Self {
        let (category, level) = objective_metadata(title);
        Self {
            title: title.to_string(),
            category,
            level,
            description: Vec::new(),
            requirement: Vec::new(),
            verification: Vec::new(),
            success_criteria: Vec::new(),
        }
    }

    fn finish(self) -> Objective {
        Objective {
            id: slugify(&self.title),
            title: self.title,
            category: self.category,
            level: self.level,
            description: self.description.join(" ").trim().to_string(),
            requirement: self.requirement,
            verification: self.verification,
            success_criteria: self.success_criteria,
        }
    }
}

fn objective_metadata(title: &str) -> (ObjectiveCategory, ObjectiveLevel) {
    (categorize_objective(title), parse_objective_level(title))
}

#[derive(Clone, Copy)]
enum ObjectiveSection {
    None,
    Requirement,
    Verification,
    SuccessCriteria,
    Goal,
    Instrumentation,
    DefinitionDone,
    NonGoals,
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: reports::ObjectiveLevel
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_objective_level(title: &str) -> ObjectiveLevel {
    let title_lower = title.to_lowercase();
    if title_contains_level(title, &title_lower, "🔴", "critical") {
        ObjectiveLevel::Critical
    } else if title_contains_level(title, &title_lower, "🟠", "high") {
        ObjectiveLevel::High
    } else if title_contains_level(title, &title_lower, "🟡", "medium") {
        ObjectiveLevel::Medium
    } else {
        ObjectiveLevel::Low
    }
}

fn title_contains_level(title: &str, title_lower: &str, marker: &str, word: &str) -> bool {
    title.contains(marker) || title_lower.contains(word)
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn categorize_objective(title: &str) -> ObjectiveCategory {
    let t = title.to_lowercase();
    categorize_objective_text(&t)
}

fn categorize_objective_text(t: &str) -> ObjectiveCategory {
    if t.contains("eventbus") {
        ObjectiveCategory::EventBusIntegrity
    } else if t.contains("hook") {
        ObjectiveCategory::HookSafety
    } else if contains_any(&t, &["control flow", "control-flow", "cycle"]) {
        ObjectiveCategory::ControlFlowGuarantee
    } else if contains_any(&t, &["deterministic", "decision"]) {
        ObjectiveCategory::DecisionDeterminism
    } else if t.contains("async") {
        ObjectiveCategory::AsyncPropagation
    } else if t.contains("routing") {
        ObjectiveCategory::NoHiddenRouting
    } else if t.contains("instrumentation") {
        ObjectiveCategory::Instrumentation
    } else {
        ObjectiveCategory::Other
    }
}

fn categorize_invariant(text: &str) -> InvariantCategory {
    let t = text.to_lowercase();
    if t.contains("route") || t.contains("routing") {
        InvariantCategory::RoutingAuthority
    } else if t.contains("event") || t.contains("log") || t.contains("tick") {
        InvariantCategory::EventLogIntegrity
    } else if t.contains("policy") || t.contains("invariant") {
        InvariantCategory::PolicyGating
    } else if t.contains("deterministic") {
        InvariantCategory::Determinism
    } else if t.contains("plan") {
        InvariantCategory::Planning
    } else if t.contains("loop") || t.contains("cycle") {
        InvariantCategory::ControlLoop
    } else if t.contains("safety") || t.contains("idempotency") {
        InvariantCategory::Safety
    } else {
        InvariantCategory::Other
    }
}

fn strip_bullet(line: &str) -> String {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("- ") {
        rest.trim().to_string()
    } else if let Some(rest) = trimmed.strip_prefix("* ") {
        rest.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            if !out.ends_with('_') {
                out.push('_');
            }
        }
    }
    out.trim_matches('_').to_string()
}
